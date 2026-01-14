use anyhow::{Context as AnyhowContext, Result};
use context_indexer::ToolMeta;
use context_protocol::ToolNextAction;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use context_meaning as meaning;

use super::cpv1::{
    parse_cpv1_anchor_details, parse_cpv1_dict, parse_cpv1_evidence, parse_cpv1_steps,
    Cpv1EvidencePointer,
};
use super::cursor::{cursor_fingerprint, decode_cursor, encode_cursor, CURSOR_VERSION};
use super::schemas::evidence_fetch::EvidencePointer;
use super::schemas::response_mode::ResponseMode;
use super::schemas::worktree_pack::{
    WorktreeInfo, WorktreePackCursorV1, WorktreePackRequest, WorktreePackResult,
    WorktreePurposeAnchor, WorktreePurposeStep, WorktreePurposeSummary,
};

const VERSION: u32 = 1;
const DEFAULT_MAX_CHARS: usize = 2_000;
const MIN_MAX_CHARS: usize = 800;
const MAX_MAX_CHARS: usize = 500_000;

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 200;

const MAX_DIRTY_PATHS_PER_WORKTREE: usize = 6;
const MAX_BRANCH_DIFF_PATHS_PER_WORKTREE: usize = 40;
const MAX_HEAD_SUBJECT_CHARS: usize = 96;
const MAX_PURPOSE_LABEL_CHARS: usize = 120;

const PURPOSE_SUMMARY_QUERY: &str =
    "canon loop (run/test/verify), CI gates, contracts, entrypoints, artifacts";
const PURPOSE_MAX_WORKTREES_PER_PAGE: usize = 5;
const PURPOSE_MEANING_MAX_CHARS: usize = 3_000;
const PURPOSE_MAX_CANON_STEPS: usize = 4;
const PURPOSE_MAX_ANCHORS: usize = 6;

#[derive(Debug, Clone)]
struct GitWorktreeInfo {
    path: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    detached: bool,
}

fn normalize_query(query: Option<&str>) -> Option<String> {
    query
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn short_branch_name(branch: &str) -> String {
    branch
        .trim()
        .strip_prefix("refs/heads/")
        .unwrap_or(branch.trim())
        .to_string()
}

fn display_worktree_path(root: &Path, worktree: &Path) -> String {
    if let Ok(rel) = worktree.strip_prefix(root) {
        let rel = rel.to_string_lossy().to_string();
        if rel.is_empty() {
            ".".to_string()
        } else {
            rel
        }
    } else {
        worktree.to_string_lossy().to_string()
    }
}

fn worktree_name(worktree: &Path) -> Option<String> {
    worktree
        .file_name()
        .map(|v| v.to_string_lossy().to_string())
        .filter(|s| !s.trim().is_empty())
}

async fn git_worktree_list(root: &Path) -> Option<Vec<GitWorktreeInfo>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("worktree")
        .arg("list")
        .arg("--porcelain")
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut out_worktrees: Vec<GitWorktreeInfo> = Vec::new();
    let mut current: Option<GitWorktreeInfo> = None;

    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(prev) = current.take() {
                out_worktrees.push(prev);
            }
            current = Some(GitWorktreeInfo {
                path: PathBuf::from(path.trim()),
                head: None,
                branch: None,
                detached: false,
            });
            continue;
        }
        let Some(ref mut wt) = current else {
            continue;
        };
        if let Some(head) = line.strip_prefix("HEAD ") {
            let head = head.trim();
            if !head.is_empty() {
                wt.head = Some(head.to_string());
            }
            continue;
        }
        if let Some(branch) = line.strip_prefix("branch ") {
            let branch = branch.trim();
            if !branch.is_empty() {
                wt.branch = Some(short_branch_name(branch));
            }
            continue;
        }
        if line == "detached" {
            wt.detached = true;
        }
    }
    if let Some(last) = current.take() {
        out_worktrees.push(last);
    }
    if out_worktrees.is_empty() {
        None
    } else {
        Some(out_worktrees)
    }
}

async fn git_head_subject(worktree: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .arg("log")
        .arg("-1")
        .arg("--pretty=%s")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let subject = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if subject.is_empty() {
        None
    } else {
        Some(subject)
    }
}

async fn git_head_short(worktree: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .arg("rev-parse")
        .arg("--short")
        .arg("HEAD")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if head.is_empty() {
        None
    } else {
        Some(head)
    }
}

async fn git_dirty_paths(worktree: &Path, limit: usize) -> Option<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .arg("status")
        .arg("--porcelain")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut paths: Vec<String> = Vec::new();
    for raw in text.lines() {
        if paths.len() >= limit {
            break;
        }
        // Porcelain format: XY <path> (renames: <old> -> <new>)
        let line = raw.trim_end_matches('\r');
        if line.len() < 4 {
            continue;
        }
        let rest = line[3..].trim();
        if rest.is_empty() {
            continue;
        }
        let path = if let Some((_, new)) = rest.rsplit_once("->") {
            new.trim()
        } else {
            rest
        };
        if path == ".worktrees" || path.starts_with(".worktrees/") {
            continue;
        }
        if !path.is_empty() {
            paths.push(path.to_string());
        }
    }
    Some(paths)
}

async fn git_symbolic_ref(worktree: &Path, reference: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .arg("symbolic-ref")
        .arg("--quiet")
        .arg(reference)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

async fn git_ref_exists(worktree: &Path, reference: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(worktree)
        .arg("show-ref")
        .arg("--verify")
        .arg("--quiet")
        .arg(reference)
        .status()
        .await
        .is_ok_and(|s| s.success())
}

async fn pick_default_base_ref(worktree: &Path) -> Option<String> {
    if let Some(origin_head) = git_symbolic_ref(worktree, "refs/remotes/origin/HEAD").await {
        return Some(origin_head);
    }
    for candidate in [
        "refs/remotes/origin/main",
        "refs/remotes/origin/master",
        "refs/heads/main",
        "refs/heads/master",
    ] {
        if git_ref_exists(worktree, candidate).await {
            return Some(candidate.to_string());
        }
    }
    None
}

async fn git_merge_base(worktree: &Path, a: &str, b: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .arg("merge-base")
        .arg(a)
        .arg(b)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

async fn git_changed_paths_against_base(
    worktree: &Path,
    base_ref: &str,
    limit: usize,
) -> Option<Vec<String>> {
    let merge_base = git_merge_base(worktree, "HEAD", base_ref).await?;
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .arg("diff")
        .arg("--name-only")
        .arg("--diff-filter=ACMRT")
        .arg(format!("{merge_base}..HEAD"))
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut paths: Vec<String> = Vec::new();
    for raw in text.lines() {
        if paths.len() >= limit {
            break;
        }
        let path = raw.trim_end_matches('\r').trim();
        if path.is_empty() {
            continue;
        }
        if path == ".worktrees" || path.starts_with(".worktrees/") {
            continue;
        }
        paths.push(path.to_string());
    }
    Some(paths)
}

fn query_score(query: &str, worktree: &WorktreeInfo) -> i32 {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return 0;
    }
    let hay = [
        worktree.name.as_deref().unwrap_or(""),
        worktree.branch.as_deref().unwrap_or(""),
        worktree.head_subject.as_deref().unwrap_or(""),
        worktree.path.as_str(),
    ]
    .join(" ")
    .to_lowercase();
    let mut score = 0i32;
    for token in query.split_whitespace().filter(|t| t.len() >= 2) {
        if hay.contains(token) {
            score += 3;
        }
    }
    if worktree.dirty.unwrap_or(false) {
        score += 2;
    }
    score
}

fn clamp_max_chars(max_chars: Option<usize>) -> usize {
    max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS)
}

fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

fn worktree_pack_content_budget(max_chars: usize) -> usize {
    const MIN_CONTENT_CHARS: usize = 160;
    const MAX_RESERVE_CHARS: usize = 4_096;

    // Reserve headroom for `.context` envelope lines + provenance note + an optional cursor.
    let base_reserve = 220usize;
    let proportional = max_chars / 18;
    let mut reserve = base_reserve.max(proportional).min(MAX_RESERVE_CHARS);
    reserve = reserve.min(max_chars.saturating_sub(MIN_CONTENT_CHARS));
    max_chars.saturating_sub(reserve).max(1)
}

fn step_kind_rank(kind: &str) -> usize {
    match kind {
        "setup" => 0,
        "build" => 1,
        "run" => 2,
        "test" => 3,
        "eval" => 4,
        "lint" => 5,
        "format" => 6,
        _ => 99,
    }
}

fn anchor_kind_rank(kind: &str) -> usize {
    match kind {
        "ci" => 0,
        "contract" => 1,
        "entrypoint" => 2,
        "artifact" => 3,
        "infra" => 4,
        "howto" => 5,
        "experiment" => 6,
        "canon" => 7,
        _ => 99,
    }
}

fn truncate_label(value: &str) -> String {
    let value = value.trim();
    if value.chars().count() <= MAX_PURPOSE_LABEL_CHARS {
        value.to_string()
    } else {
        let mut out = crate::tools::util::truncate_to_chars(value, MAX_PURPOSE_LABEL_CHARS);
        out.push('…');
        out
    }
}

fn ev_to_pointer(ev: &Cpv1EvidencePointer) -> EvidencePointer {
    EvidencePointer {
        file: ev.file.clone(),
        start_line: ev.start_line,
        end_line: ev.end_line,
        source_hash: ev.source_hash.clone(),
    }
}

async fn compute_worktree_purpose_summary(
    worktree_root: &Path,
    worktree_display: &str,
    dirty_paths: Option<&[String]>,
    base_ref: Option<&str>,
) -> Option<WorktreePurposeSummary> {
    if !worktree_root.is_dir() {
        return None;
    }

    let request = meaning::MeaningPackRequest {
        query: PURPOSE_SUMMARY_QUERY.to_string(),
        map_depth: Some(2),
        map_limit: Some(10),
        max_chars: Some(PURPOSE_MEANING_MAX_CHARS),
    };

    let engine = meaning::meaning_pack(worktree_root, worktree_display, &request)
        .await
        .ok()?;

    let dict = parse_cpv1_dict(&engine.pack);
    let ev_map = parse_cpv1_evidence(&engine.pack, &dict);

    let mut steps = parse_cpv1_steps(&engine.pack, &dict);
    steps.sort_by_key(|s| step_kind_rank(&s.kind));
    let canon: Vec<WorktreePurposeStep> = steps
        .into_iter()
        .take(PURPOSE_MAX_CANON_STEPS)
        .filter_map(|s| {
            let ev = ev_map.get(&s.ev)?;
            Some(WorktreePurposeStep {
                kind: s.kind,
                label: truncate_label(&s.label),
                confidence: s.confidence,
                evidence: Some(ev_to_pointer(ev)),
            })
        })
        .collect();

    let mut anchors = parse_cpv1_anchor_details(&engine.pack, &dict);
    anchors.sort_by_key(|a| anchor_kind_rank(&a.kind));
    let anchors: Vec<WorktreePurposeAnchor> = anchors
        .into_iter()
        .take(PURPOSE_MAX_ANCHORS)
        .filter_map(|a| {
            let ev = ev_map.get(&a.ev)?;
            Some(WorktreePurposeAnchor {
                kind: a.kind,
                label: a.label.map(|v| truncate_label(&v)),
                file: a.file.map(|v| truncate_label(&v)),
                confidence: a.confidence,
                evidence: Some(ev_to_pointer(ev)),
            })
        })
        .collect();

    fn area_kind_rank(kind: &str) -> usize {
        match kind {
            "interfaces" => 0,
            "core" => 1,
            "ci" => 2,
            "docs" => 3,
            "tooling" => 4,
            "infra" => 5,
            "outputs" => 6,
            "experiments" => 7,
            _ => 99,
        }
    }

    let mut touched_areas: Vec<String> = Vec::new();
    let diff_paths = if let Some(base_ref) = base_ref {
        git_changed_paths_against_base(worktree_root, base_ref, MAX_BRANCH_DIFF_PATHS_PER_WORKTREE)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if dirty_paths.is_some_and(|v| !v.is_empty()) || !diff_paths.is_empty() {
        fn classify_dirty_path(path: &str) -> Option<&'static str> {
            let lc = path.trim().to_ascii_lowercase();
            if lc.is_empty() {
                return None;
            }

            // Worktree stores are workspace noise; don't let them claim a zone.
            if lc == ".worktrees" || lc.starts_with(".worktrees/") {
                return None;
            }

            // Contracts/protocols are the most important "what changed" signal.
            let basename = lc.rsplit('/').next().unwrap_or(lc.as_str());
            let is_dir_marker = lc.ends_with('/') || basename.is_empty();
            let is_contract_like_ext = lc.ends_with(".proto")
                || lc.ends_with(".avsc")
                || lc.ends_with(".yaml")
                || lc.ends_with(".yml")
                || lc.ends_with(".json")
                || lc.ends_with(".toml")
                || lc.ends_with(".md")
                || lc.ends_with(".rst")
                || lc.ends_with(".txt");
            let is_contract_dir = lc.starts_with("contracts/")
                || lc.starts_with("proto/")
                || lc.starts_with("docs/contracts/")
                || lc.starts_with("docs/contract/")
                || lc.starts_with("docs/spec/")
                || lc.starts_with("docs/specs/")
                || lc.starts_with("docs/protocol/")
                || lc.starts_with("docs/protocols/")
                || lc.contains("/contracts/")
                || lc.contains("/contract/")
                || lc.contains("/schemas/")
                || lc.contains("/schema/")
                || lc.contains("/specs/")
                || lc.contains("/spec/")
                || lc.contains("/protocols/")
                || lc.contains("/protocol/");
            if is_contract_dir
                && (is_contract_like_ext || is_dir_marker)
                && !matches!(
                    basename,
                    "readme.md" | "readme.rst" | "readme.txt" | "index.md"
                )
            {
                return Some("interfaces");
            }
            if basename == ".gitlab-ci.yml"
                || basename == "jenkinsfile"
                || lc.starts_with(".github/workflows/")
            {
                return Some("ci");
            }
            if lc.starts_with("k8s/")
                || lc.contains("/k8s/")
                || lc.starts_with("kubernetes/")
                || lc.contains("/kubernetes/")
                || lc.starts_with("manifests/")
                || lc.contains("/manifests/")
                || lc.starts_with("gitops/")
                || lc.contains("/gitops/")
                || lc.starts_with("infra/")
                || lc.contains("/infra/")
                || lc.contains("/terraform/")
                || lc.starts_with("terraform/")
                || lc.contains("/charts/")
                || lc.starts_with("charts/")
            {
                return Some("infra");
            }

            // Artifacts/outputs (research + ML repos).
            let first = lc.split('/').next().unwrap_or("").trim();
            if matches!(
                first,
                "artifacts"
                    | "artifact"
                    | "results"
                    | "runs"
                    | "outputs"
                    | "output"
                    | "checkpoints"
                    | "checkpoint"
                    | "data"
                    | "dataset"
                    | "datasets"
                    | "corpus"
                    | "corpora"
                    | "weights"
            ) {
                return Some("outputs");
            }

            if lc.starts_with("experiments/")
                || lc.starts_with("experiment/")
                || lc.starts_with("baselines/")
                || lc.starts_with("baseline/")
                || lc.starts_with("bench/")
                || lc.starts_with("benches/")
                || lc.contains("/eval/")
                || lc.contains("/evaluation/")
                || lc.contains("/analysis/")
                || lc.contains("/notebooks/")
            {
                return Some("experiments");
            }

            if basename == "makefile"
                || basename == "justfile"
                || basename == "cargo.toml"
                || basename == "package.json"
                || basename == "pyproject.toml"
                || lc.starts_with("scripts/")
            {
                return Some("tooling");
            }

            // Code files (best-effort, extensions only to keep it cheap).
            if matches!(
                lc.rsplit('.').next().unwrap_or(""),
                "rs" | "py"
                    | "js"
                    | "ts"
                    | "tsx"
                    | "go"
                    | "java"
                    | "kt"
                    | "c"
                    | "cc"
                    | "cpp"
                    | "cxx"
                    | "h"
                    | "hpp"
                    | "cs"
                    | "rb"
                    | "php"
                    | "swift"
            ) {
                return Some("core");
            }

            if lc.starts_with("docs/")
                || matches!(lc.rsplit('.').next().unwrap_or(""), "md" | "rst" | "txt")
            {
                return Some("docs");
            }

            None
        }

        let mut touched: HashSet<&'static str> = HashSet::new();
        if let Some(paths) = dirty_paths {
            for dirty in paths {
                if let Some(kind) = classify_dirty_path(dirty) {
                    touched.insert(kind);
                }
            }
        }
        for changed in &diff_paths {
            if let Some(kind) = classify_dirty_path(changed) {
                touched.insert(kind);
            }
        }

        let mut rendered = touched.into_iter().collect::<Vec<_>>();
        rendered.sort_by(|a, b| area_kind_rank(a).cmp(&area_kind_rank(b)).then(a.cmp(b)));
        touched_areas = rendered
            .into_iter()
            .take(5)
            .map(|v| v.to_string())
            .collect();
    }

    if canon.is_empty() && anchors.is_empty() {
        return None;
    }

    Some(WorktreePurposeSummary {
        canon,
        anchors,
        touched_areas,
        meaning_truncated: Some(engine.budget.truncated),
    })
}

fn render_worktree_lines(worktree: &WorktreeInfo) -> Vec<String> {
    let mut lines = Vec::new();
    let display_path = worktree
        .display_path
        .as_deref()
        .unwrap_or(worktree.path.as_str());
    let mut line = format!("WT path={display_path}");
    if let Some(branch) = worktree.branch.as_deref() {
        line.push_str(&format!(" branch={branch}"));
    }
    if let Some(head) = worktree.head.as_deref() {
        line.push_str(&format!(" head={head}"));
    }
    if let Some(subject) = worktree.head_subject.as_deref() {
        let mut subject = subject.trim().to_string();
        if subject.chars().count() > MAX_HEAD_SUBJECT_CHARS {
            subject = crate::tools::util::truncate_to_chars(&subject, MAX_HEAD_SUBJECT_CHARS);
            subject.push('…');
        }
        if !subject.is_empty() {
            line.push_str(&format!(" subject={}", serde_json::json!(subject)));
        }
    }
    if let Some(dirty) = worktree.dirty {
        line.push_str(if dirty { " dirty=1" } else { " dirty=0" });
    }
    lines.push(line);
    if let Some(paths) = worktree.dirty_paths.as_ref() {
        if !paths.is_empty() {
            let joined = paths
                .iter()
                .take(MAX_DIRTY_PATHS_PER_WORKTREE)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("  dirty_paths: {joined}"));
        }
    }
    if let Some(purpose) = worktree.purpose.as_ref() {
        if !purpose.canon.is_empty() {
            let rendered = purpose
                .canon
                .iter()
                .map(|step| format!("{}={}", step.kind, serde_json::json!(step.label)))
                .collect::<Vec<_>>()
                .join("; ");
            lines.push(format!("  canon: {rendered}"));
        }
        if !purpose.anchors.is_empty() {
            let rendered = purpose
                .anchors
                .iter()
                .map(|anchor| {
                    let v = anchor
                        .file
                        .as_deref()
                        .or(anchor.label.as_deref())
                        .unwrap_or("");
                    if v.is_empty() {
                        anchor.kind.clone()
                    } else {
                        format!("{}={}", anchor.kind, serde_json::json!(v))
                    }
                })
                .collect::<Vec<_>>()
                .join("; ");
            if !rendered.trim().is_empty() {
                lines.push(format!("  anchors: {rendered}"));
            }
        }
        if !purpose.touched_areas.is_empty() {
            let rendered = purpose.touched_areas.join("; ");
            if !rendered.trim().is_empty() {
                lines.push(format!("  touches: {rendered}"));
            }
        }
        if purpose.meaning_truncated.unwrap_or(false) {
            lines.push("  purpose_truncated=1".to_string());
        }
    }
    lines
}

pub(super) fn decode_worktree_pack_cursor(cursor: &str) -> Result<WorktreePackCursorV1> {
    decode_cursor(cursor).with_context(|| "decode worktree_pack cursor")
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn compute_worktree_pack_result(
    root: &Path,
    root_display: &str,
    request: &WorktreePackRequest,
    cursor: Option<&str>,
) -> Result<WorktreePackResult> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Minimal);
    let max_chars = clamp_max_chars(request.max_chars);
    let limit = clamp_limit(request.limit);
    let query = normalize_query(request.query.as_deref());

    let mut offset: usize = 0;
    if let Some(cursor) = cursor {
        let decoded = decode_worktree_pack_cursor(cursor)?;
        if decoded.v != CURSOR_VERSION || decoded.tool != "worktree_pack" {
            anyhow::bail!("Invalid cursor: wrong tool/version");
        }
        if let Some(root_hash) = decoded.root_hash {
            let expected = cursor_fingerprint(root_display);
            if root_hash != expected {
                anyhow::bail!("Invalid cursor: different root");
            }
        }
        if decoded.limit != 0 && decoded.limit != limit {
            anyhow::bail!("Invalid cursor: different limit");
        }
        if decoded.query != query {
            anyhow::bail!("Invalid cursor: different query");
        }
        offset = decoded.offset;
    }

    let git_worktrees = git_worktree_list(root).await.unwrap_or_else(|| {
        vec![GitWorktreeInfo {
            path: root.to_path_buf(),
            head: None,
            branch: None,
            detached: false,
        }]
    });

    let mut worktrees: Vec<WorktreeInfo> = Vec::new();
    for wt in git_worktrees {
        let absolute_path = wt.path.to_string_lossy().to_string();
        let display_candidate = display_worktree_path(root, &wt.path);
        let display_path = if display_candidate != absolute_path {
            Some(display_candidate)
        } else {
            None
        };
        let name = worktree_name(&wt.path);
        let branch = if wt.detached { None } else { wt.branch.clone() };
        // HEAD short + subject come from the worktree itself (best-effort).
        let head_short = git_head_short(&wt.path).await;
        let head_subject = git_head_subject(&wt.path).await;
        let dirty_paths = git_dirty_paths(&wt.path, MAX_DIRTY_PATHS_PER_WORKTREE).await;
        let dirty = dirty_paths.as_ref().map(|v| !v.is_empty());
        let head = head_short.or_else(|| {
            wt.head.as_deref().and_then(|value| {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.chars().take(12).collect::<String>())
                }
            })
        });
        worktrees.push(WorktreeInfo {
            path: absolute_path,
            display_path,
            name,
            branch,
            head,
            head_subject,
            dirty,
            dirty_paths,
            purpose: None,
        });
    }

    // Deterministic ranking: query relevance (if present) → dirty-first → stable path.
    if let Some(q) = query.as_deref() {
        worktrees.sort_by(|a, b| {
            let sa = query_score(q, a);
            let sb = query_score(q, b);
            sb.cmp(&sa)
                .then_with(|| b.dirty.unwrap_or(false).cmp(&a.dirty.unwrap_or(false)))
                .then_with(|| a.path.cmp(&b.path))
        });
    } else {
        worktrees.sort_by(|a, b| {
            b.dirty
                .unwrap_or(false)
                .cmp(&a.dirty.unwrap_or(false))
                .then_with(|| a.path.cmp(&b.path))
        });
    }

    let total_worktrees = worktrees.len();
    let mut returned: Vec<WorktreeInfo> = Vec::new();
    let mut used_chars: usize = 0;
    let content_budget = worktree_pack_content_budget(max_chars);
    let mut next_offset = offset;

    for wt in worktrees.into_iter().skip(offset) {
        if returned.len() >= limit {
            break;
        }
        let rendered_lines = render_worktree_lines(&wt);
        let rendered_chars = rendered_lines
            .iter()
            .map(|line| line.chars().count().saturating_add(1))
            .sum::<usize>();
        if !returned.is_empty() && used_chars.saturating_add(rendered_chars) > content_budget {
            break;
        }
        used_chars = used_chars.saturating_add(rendered_chars);
        returned.push(wt);
        next_offset = next_offset.saturating_add(1);
    }

    let truncated = next_offset < total_worktrees;
    let next_cursor = if truncated {
        let cursor = WorktreePackCursorV1 {
            v: CURSOR_VERSION,
            tool: "worktree_pack".to_string(),
            root: None,
            root_hash: Some(cursor_fingerprint(root_display)),
            limit,
            offset: next_offset,
            query: query.clone(),
        };
        encode_cursor(&cursor).ok()
    } else {
        None
    };

    // Optional (full mode): attach a small, evidence-backed purpose summary per worktree.
    if response_mode == ResponseMode::Full {
        let base_ref = pick_default_base_ref(root).await;
        for (idx, wt) in returned.iter_mut().enumerate() {
            if idx >= PURPOSE_MAX_WORKTREES_PER_PAGE {
                break;
            }
            let worktree_root = Path::new(&wt.path);
            wt.purpose = compute_worktree_purpose_summary(
                worktree_root,
                wt.path.as_str(),
                wt.dirty_paths.as_deref(),
                base_ref.as_deref(),
            )
            .await;
        }

        // Budget guard: if purpose summaries blow the `.context` budget, drop them from the tail.
        let recompute_used_chars = |items: &[WorktreeInfo]| -> usize {
            items
                .iter()
                .flat_map(render_worktree_lines)
                .map(|line| line.chars().count().saturating_add(1))
                .sum::<usize>()
        };

        let mut used = recompute_used_chars(&returned);
        if used > content_budget {
            for idx in (0..returned.len()).rev() {
                if returned[idx].purpose.take().is_some() {
                    used = recompute_used_chars(&returned);
                    if used <= content_budget {
                        break;
                    }
                }
            }
        }
        used_chars = used;
    }

    let next_actions = if response_mode == ResponseMode::Full {
        let mut actions: Vec<ToolNextAction> = Vec::new();
        if let Some(best) = returned.first() {
            // Suggest drilling into the highest-ranked worktree.
            let drill_query = request
                .query
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| PURPOSE_SUMMARY_QUERY.to_string());
            actions.push(ToolNextAction {
                tool: "meaning_pack".to_string(),
                args: serde_json::json!({
                    "path": best.path.clone(),
                    "query": drill_query,
                    "max_chars": 2000,
                    "response_mode": "full",
                }),
                reason: "Drill into the most relevant worktree with a meanings-first pack"
                    .to_string(),
            });

            if let Some(purpose) = best.purpose.as_ref() {
                let mut items: Vec<EvidencePointer> = Vec::new();
                let mut seen: std::collections::BTreeSet<(String, usize, usize, Option<String>)> =
                    std::collections::BTreeSet::new();

                for step in &purpose.canon {
                    if let Some(ev) = step.evidence.as_ref() {
                        let key = (
                            ev.file.clone(),
                            ev.start_line,
                            ev.end_line,
                            ev.source_hash.clone(),
                        );
                        if seen.insert(key) {
                            items.push(ev.clone());
                        }
                    }
                }
                for anchor in &purpose.anchors {
                    if items.len() >= 4 {
                        break;
                    }
                    if let Some(ev) = anchor.evidence.as_ref() {
                        let key = (
                            ev.file.clone(),
                            ev.start_line,
                            ev.end_line,
                            ev.source_hash.clone(),
                        );
                        if seen.insert(key) {
                            items.push(ev.clone());
                        }
                    }
                }
                if !items.is_empty() {
                    actions.push(ToolNextAction {
                        tool: "evidence_fetch".to_string(),
                        args: serde_json::json!({
                            "path": best.path.clone(),
                            "items": items,
                            "max_chars": 2000,
                            "max_lines": 200,
                            "response_mode": "facts",
                        }),
                        reason: "Fetch evidence for canon/CI/contracts claims in the top worktree"
                            .to_string(),
                    });
                }
            }
        }
        if truncated {
            if let Some(cursor) = next_cursor.clone() {
                actions.push(ToolNextAction {
                    tool: "worktree_pack".to_string(),
                    args: serde_json::json!({
                        "path": request.path,
                        "cursor": cursor,
                        "response_mode": "facts",
                    }),
                    reason: "Continue listing worktrees via cursor pagination".to_string(),
                });
            }
        }
        if actions.is_empty() {
            None
        } else {
            Some(actions)
        }
    } else {
        None
    };

    Ok(WorktreePackResult {
        total_worktrees: if response_mode == ResponseMode::Minimal {
            None
        } else {
            Some(total_worktrees)
        },
        returned: if response_mode == ResponseMode::Minimal {
            None
        } else {
            Some(returned.len())
        },
        used_chars: if response_mode == ResponseMode::Minimal {
            None
        } else {
            Some(used_chars)
        },
        limit: if response_mode == ResponseMode::Minimal {
            None
        } else {
            Some(limit)
        },
        max_chars: if response_mode == ResponseMode::Minimal {
            None
        } else {
            Some(max_chars)
        },
        truncated,
        next_cursor,
        next_actions,
        meta: Some(ToolMeta::default()),
        worktrees: returned,
    })
}

pub(super) fn render_worktree_pack_block(result: &WorktreePackResult) -> String {
    let mut out = String::new();
    out.push_str(&format!("WTV{VERSION}\n"));
    for wt in &result.worktrees {
        for line in render_worktree_lines(wt) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}
