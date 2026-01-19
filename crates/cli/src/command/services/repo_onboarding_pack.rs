use crate::command::context::{unix_ms, CommandContext};
use crate::command::domain::{
    parse_payload, CommandOutcome, Hint, MapOutput, MapPayload, RepoOnboardingDocSlice,
    RepoOnboardingDocsReason, RepoOnboardingPackBudget, RepoOnboardingPackOutput,
    RepoOnboardingPackPayload,
};
use crate::command::freshness;
use anyhow::{Context as AnyhowContext, Result};
use context_protocol::{enforce_max_chars, finalize_used_chars, BudgetTruncation, DefaultBudgets};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use super::context::ContextService;

const VERSION: u32 = 1;
const DEFAULT_MAX_CHARS: usize = 20_000;
const MIN_MAX_CHARS: usize = 1_000;
const MAX_MAX_CHARS: usize = 500_000;
const DEFAULT_MAP_DEPTH: usize = 2;
const DEFAULT_MAP_LIMIT: usize = 20;
const DEFAULT_DOCS_LIMIT: usize = 8;
const MAX_DOCS_LIMIT: usize = 25;
const DEFAULT_DOC_MAX_LINES: usize = 200;
const MAX_DOC_MAX_LINES: usize = 5_000;
const DEFAULT_DOC_MAX_CHARS: usize = 6_000;
const MAX_DOC_MAX_CHARS: usize = 100_000;

const DEFAULT_AUTO_INDEX_BUDGET_MS: u64 = 15_000;
const MIN_AUTO_INDEX_BUDGET_MS: u64 = 100;
const MAX_AUTO_INDEX_BUDGET_MS: u64 = 120_000;

const DEFAULT_DOC_CANDIDATES: &[&str] = &[
    "AGENTS.md",
    "README.md",
    "docs/QUICK_START.md",
    "contracts/README.md",
    "docs/README.md",
    "USAGE_EXAMPLES.md",
    "PHILOSOPHY.md",
    "CONTRIBUTING.md",
    "docs/COMMAND_RFC.md",
];

#[derive(Clone, Copy, Debug)]
struct AutoIndexPolicy {
    enabled: bool,
    budget_ms: u64,
}

impl AutoIndexPolicy {
    fn from_request(auto_index: Option<bool>, auto_index_budget_ms: Option<u64>) -> Self {
        let enabled = auto_index.unwrap_or(true);
        let budget_ms = auto_index_budget_ms
            .unwrap_or(DEFAULT_AUTO_INDEX_BUDGET_MS)
            .clamp(MIN_AUTO_INDEX_BUDGET_MS, MAX_AUTO_INDEX_BUDGET_MS);
        Self { enabled, budget_ms }
    }
}

pub struct RepoOnboardingPackService;

impl RepoOnboardingPackService {
    pub async fn run(
        &self,
        payload: serde_json::Value,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome> {
        let payload: RepoOnboardingPackPayload = parse_payload(payload)?;
        let project_ctx = ctx.resolve_project(payload.project.clone()).await?;
        let _ = crate::heartbeat::ping(&project_ctx.root).await;

        let policy =
            AutoIndexPolicy::from_request(payload.auto_index, payload.auto_index_budget_ms);
        let mut index_state =
            freshness::gather_index_state(&project_ctx.root, &project_ctx.profile_name).await?;
        let mut reindex_hints = Vec::new();
        let mut index_updated = false;
        if policy.enabled && (index_state.stale || !index_state.index.exists) {
            let attempt = freshness::attempt_reindex(
                &project_ctx.root,
                &project_ctx.profile,
                policy.budget_ms,
            )
            .await;
            reindex_hints.push(freshness::render_reindex_hint(&attempt));
            index_updated = attempt.performed;
            if let Ok(refreshed) =
                freshness::gather_index_state(&project_ctx.root, &project_ctx.profile_name).await
            {
                index_state = refreshed;
            }
            index_state.reindex = Some(attempt);
        }

        let map_depth = payload.map_depth.unwrap_or(DEFAULT_MAP_DEPTH).clamp(1, 4);
        let map_limit = payload.map_limit.unwrap_or(DEFAULT_MAP_LIMIT).clamp(1, 200);

        let map_outcome = build_map_output(&project_ctx.root, map_depth, map_limit, ctx).await?;

        let docs_limit = payload
            .docs_limit
            .unwrap_or(DEFAULT_DOCS_LIMIT)
            .clamp(0, MAX_DOCS_LIMIT);
        let doc_max_lines = payload
            .doc_max_lines
            .unwrap_or(DEFAULT_DOC_MAX_LINES)
            .clamp(1, MAX_DOC_MAX_LINES);
        let doc_max_chars = payload
            .doc_max_chars
            .unwrap_or(DEFAULT_DOC_MAX_CHARS)
            .clamp(1, MAX_DOC_MAX_CHARS);
        let max_chars = payload
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);

        let has_index = index_state.index.exists;
        let doc_candidates = collect_doc_candidates(&payload);
        let root_display = project_ctx.root.display().to_string();

        let mut result = RepoOnboardingPackOutput {
            version: VERSION,
            root: root_display.clone(),
            map: map_outcome.map,
            docs: Vec::new(),
            omitted_doc_paths: Vec::new(),
            docs_reason: None,
            next_actions: build_next_actions(&root_display, has_index),
            budget: RepoOnboardingPackBudget {
                max_chars,
                used_chars: 0,
                truncated: false,
                truncation: None,
            },
        };

        add_docs_best_effort(
            &mut result,
            &project_ctx.root,
            &doc_candidates,
            docs_limit,
            doc_max_lines,
            doc_max_chars,
        )?;
        trim_to_budget(&mut result)?;
        if result.docs.is_empty() {
            result.docs_reason = Some(if docs_limit == 0 {
                RepoOnboardingDocsReason::DocsLimitZero
            } else if doc_candidates.is_empty() {
                RepoOnboardingDocsReason::NoDocCandidates
            } else if matches!(result.budget.truncation, Some(BudgetTruncation::MaxChars)) {
                RepoOnboardingDocsReason::MaxChars
            } else {
                RepoOnboardingDocsReason::DocsNotFound
            });
        }

        if result.budget.truncated && !doc_candidates.is_empty() {
            let included: HashSet<&str> = result.docs.iter().map(|d| d.file.as_str()).collect();
            let last_included_idx = doc_candidates
                .iter()
                .enumerate()
                .filter_map(|(idx, cand)| included.contains(cand.as_str()).then_some(idx))
                .next_back();
            let start_idx = last_included_idx.map(|idx| idx + 1).unwrap_or(0);
            result.omitted_doc_paths = doc_candidates.iter().skip(start_idx).cloned().collect();
        }

        let mut outcome = CommandOutcome::from_value(result)?;
        outcome.hints.extend(map_outcome.hints);
        outcome.hints.extend(reindex_hints);
        outcome.hints.extend(project_ctx.hints);
        outcome.meta = map_outcome.meta;
        outcome.meta.config_path = project_ctx.config_path;
        outcome.meta.profile = Some(project_ctx.profile_name.clone());
        outcome.meta.profile_path = project_ctx.profile_path;
        outcome.meta.index_updated = Some(index_updated);
        outcome.meta.index_state = Some(index_state);
        Ok(outcome)
    }
}

struct MapOutcome {
    map: MapOutput,
    meta: crate::command::domain::ResponseMeta,
    hints: Vec<Hint>,
}

async fn build_map_output(
    root: &Path,
    depth: usize,
    limit: usize,
    ctx: &CommandContext,
) -> Result<MapOutcome> {
    let payload = MapPayload {
        project: Some(root.to_path_buf()),
        depth,
        limit: Some(limit),
    };
    let context_service = ContextService;
    let map_outcome = context_service
        .map(serde_json::to_value(payload)?, ctx)
        .await?;
    let map: MapOutput = serde_json::from_value(map_outcome.data.clone())
        .context("Invalid map output (expected MapOutput)")?;
    Ok(MapOutcome {
        map,
        meta: map_outcome.meta,
        hints: map_outcome.hints,
    })
}

fn build_next_actions(
    root_display: &str,
    has_index: bool,
) -> Vec<context_protocol::ToolNextAction> {
    let budgets = DefaultBudgets::default();
    let mut next_actions = Vec::new();

    if !has_index {
        next_actions.push(context_protocol::ToolNextAction {
            tool: "index".to_string(),
            args: serde_json::json!({ "path": root_display }),
            reason: "Build the semantic index (required for semantic search and packs)."
                .to_string(),
        });
    }

    next_actions.push(context_protocol::ToolNextAction {
        tool: "text_search".to_string(),
        args: serde_json::json!({
            "project": root_display,
            "pattern": "TODO",
            "max_results": 200
        }),
        reason: "Scan for TODO markers across the repo (literal search).".to_string(),
    });

    if has_index {
        next_actions.push(context_protocol::ToolNextAction {
            tool: "context_pack".to_string(),
            args: serde_json::json!({
                "project": root_display,
                "query": "describe what you want to change",
                "max_chars": budgets.context_pack_max_chars
            }),
            reason: "One-shot semantic onboarding pack for a concrete question.".to_string(),
        });
    }

    next_actions
}

fn collect_doc_candidates(request: &RepoOnboardingPackPayload) -> Vec<String> {
    if let Some(custom) = request.doc_paths.as_ref() {
        let mut seen = HashSet::new();
        let mut doc_candidates: Vec<String> = Vec::new();
        for rel in custom {
            let rel = rel.trim();
            if rel.is_empty() {
                continue;
            }
            let rel = rel.replace('\\', "/");
            if seen.insert(rel.clone()) {
                doc_candidates.push(rel);
            }
        }
        return doc_candidates;
    }

    DEFAULT_DOC_CANDIDATES
        .iter()
        .map(|&rel| rel.to_owned())
        .collect()
}

fn add_docs_best_effort(
    result: &mut RepoOnboardingPackOutput,
    root: &Path,
    doc_candidates: &[String],
    docs_limit: usize,
    doc_max_lines: usize,
    doc_max_chars: usize,
) -> Result<()> {
    for rel in doc_candidates {
        if result.docs.len() >= docs_limit {
            result.budget.truncated = true;
            result.budget.truncation = Some(BudgetTruncation::DocsLimit);
            break;
        }

        let Ok(slice) = compute_onboarding_doc_slice(root, rel, 1, doc_max_lines, doc_max_chars)
        else {
            continue;
        };
        result.docs.push(slice);

        finalize_repo_onboarding_budget(result)?;
        if result.budget.used_chars > result.budget.max_chars {
            result.budget.truncated = true;
            result.budget.truncation = Some(BudgetTruncation::MaxChars);
            break;
        }
    }

    Ok(())
}

fn trim_to_budget(result: &mut RepoOnboardingPackOutput) -> Result<()> {
    let max_chars = result.budget.max_chars;
    let reserved_docs = if result.docs.is_empty() { 0 } else { 1 };
    let enforce = |target: &mut RepoOnboardingPackOutput, min_docs: usize| {
        enforce_max_chars(
            target,
            max_chars,
            |inner, used| inner.budget.used_chars = used,
            |inner| {
                inner.budget.truncated = true;
                inner.budget.truncation = Some(BudgetTruncation::MaxChars);
            },
            |inner| {
                if !inner.map.nodes.is_empty() {
                    inner.map.nodes.pop();
                    return true;
                }
                if inner.next_actions.len() > 1 {
                    inner.next_actions.pop();
                    return true;
                }
                if inner.docs.len() > min_docs {
                    inner.docs.pop();
                    return true;
                }
                false
            },
        )
    };

    let used = match enforce(result, reserved_docs) {
        Ok(used) => used,
        Err(err) => {
            if !result.docs.is_empty() {
                result.docs.clear();
                result.docs_reason = Some(RepoOnboardingDocsReason::MaxChars);
                enforce(result, 0)?
            } else {
                return Err(err);
            }
        }
    };
    result.budget.used_chars = used;
    Ok(())
}

fn finalize_repo_onboarding_budget(result: &mut RepoOnboardingPackOutput) -> Result<()> {
    finalize_used_chars(result, |inner, used| inner.budget.used_chars = used).map(|_| ())
}

fn compute_onboarding_doc_slice(
    root: &Path,
    file: &str,
    start_line: usize,
    max_lines: usize,
    max_chars: usize,
) -> Result<RepoOnboardingDocSlice> {
    let file = file.trim();
    if file.is_empty() {
        anyhow::bail!("Doc file path must not be empty");
    }

    let canonical_file = root
        .join(Path::new(file))
        .canonicalize()
        .with_context(|| format!("Failed to resolve doc path '{file}'"))?;
    if !canonical_file.starts_with(root) {
        anyhow::bail!("Doc file '{file}' is outside project root");
    }

    let display_file = normalize_relative_path(root, &canonical_file).unwrap_or_else(|| {
        canonical_file
            .to_string_lossy()
            .into_owned()
            .replace('\\', "/")
    });

    let meta = std::fs::metadata(&canonical_file)
        .with_context(|| format!("Failed to stat '{display_file}'"))?;
    let file_size_bytes = meta.len();
    let file_mtime_ms = meta.modified().map(unix_ms).unwrap_or(0);

    let file =
        File::open(&canonical_file).with_context(|| format!("Failed to open '{display_file}'"))?;
    let reader = BufReader::new(file);

    let mut content = String::new();
    let mut used_chars = 0usize;
    let mut returned_lines = 0usize;
    let mut end_line = 0usize;
    let mut truncated = false;
    let mut truncation: Option<BudgetTruncation> = None;

    for (idx, line) in reader.lines().enumerate() {
        let line_no = idx + 1;
        let line = line.with_context(|| format!("Failed to read '{display_file}'"))?;

        if line_no < start_line {
            continue;
        }

        if returned_lines >= max_lines {
            truncated = true;
            truncation = Some(BudgetTruncation::MaxLines);
            break;
        }

        let line_chars = line.chars().count();
        let extra_chars = if returned_lines == 0 {
            line_chars
        } else {
            1 + line_chars
        };
        if used_chars.saturating_add(extra_chars) > max_chars {
            truncated = true;
            truncation = Some(BudgetTruncation::MaxChars);
            break;
        }

        if returned_lines > 0 {
            content.push('\n');
            used_chars += 1;
        }
        content.push_str(&line);
        used_chars += line_chars;
        returned_lines += 1;
        end_line = line_no;
    }

    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let content_sha256 = hex_encode_lower(&hasher.finalize());

    Ok(RepoOnboardingDocSlice {
        file: display_file,
        start_line,
        end_line,
        returned_lines,
        used_chars,
        max_lines,
        max_chars,
        truncated,
        truncation,
        file_size_bytes,
        file_mtime_ms,
        content_sha256,
        content,
    })
}

fn normalize_relative_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let rel = rel.to_string_lossy().into_owned();
    Some(rel.replace('\\', "/"))
}

fn hex_encode_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}
