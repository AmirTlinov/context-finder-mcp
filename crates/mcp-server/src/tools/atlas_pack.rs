use anyhow::{Context as AnyhowContext, Result};
use context_indexer::ToolMeta;
use context_protocol::ToolNextAction;
use serde_json::json;
use std::path::Path;

use context_meaning as meaning;

use super::cpv1::{
    parse_cpv1_anchor_details, parse_cpv1_dict, parse_cpv1_evidence, parse_cpv1_steps,
};
use super::schemas::atlas_pack::{AtlasPackBudget, AtlasPackRequest, AtlasPackResult};
use super::schemas::evidence_fetch::EvidencePointer;
use super::schemas::response_mode::ResponseMode;
use super::schemas::worktree_pack::WorktreePackRequest;
use super::worktree_pack::compute_worktree_pack_result;
use super::{
    notebook_store::{load_or_init_notebook, notebook_paths_for_scope, resolve_repo_identity},
    notebook_types::NotebookScope,
};

const VERSION: u32 = 1;

const DEFAULT_MAX_CHARS: usize = 2_000;
const MIN_MAX_CHARS: usize = 800;
const MAX_MAX_CHARS: usize = 500_000;

const DEFAULT_WORKTREE_LIMIT: usize = 10;
const MAX_WORKTREE_LIMIT: usize = 50;

const DEFAULT_QUERY: &str =
    "canon loop (run/test/verify), CI gates, contracts, entrypoints, artifacts";

fn normalize_query(query: Option<&str>) -> Option<String> {
    query
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn clamp_max_chars(max_chars: Option<usize>) -> usize {
    max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS)
}

fn clamp_worktree_limit(limit: Option<usize>) -> usize {
    limit
        .unwrap_or(DEFAULT_WORKTREE_LIMIT)
        .clamp(1, MAX_WORKTREE_LIMIT)
}

fn pick_focus_from_pack(
    pack: &str,
    dict: &std::collections::HashMap<String, String>,
) -> Option<String> {
    for raw in pack.lines() {
        let line = raw.trim_end_matches('\r');
        if !line.starts_with("AREA ") {
            continue;
        }
        for token in line.split_whitespace() {
            if let Some(v) = token.strip_prefix("path=") {
                return dict.get(v).cloned().or_else(|| Some(v.to_string()));
            }
        }
    }
    for raw in pack.lines() {
        let line = raw.trim_end_matches('\r');
        if !line.starts_with("MAP ") {
            continue;
        }
        for token in line.split_whitespace() {
            if let Some(v) = token.strip_prefix("path=") {
                return dict.get(v).cloned().or_else(|| Some(v.to_string()));
            }
        }
    }
    None
}

fn derive_evidence_items(pack: &str) -> Vec<EvidencePointer> {
    let dict = parse_cpv1_dict(pack);
    let ev_map = parse_cpv1_evidence(pack, &dict);

    let mut items: Vec<EvidencePointer> = Vec::new();
    let mut seen: std::collections::BTreeSet<(String, usize, usize, Option<String>)> =
        std::collections::BTreeSet::new();

    let anchors = parse_cpv1_anchor_details(pack, &dict);
    for want_kind in ["ci", "contract", "entrypoint", "howto"] {
        if items.len() >= 3 {
            break;
        }
        let Some(anchor) = anchors.iter().find(|a| a.kind == want_kind) else {
            continue;
        };
        let Some(ev) = ev_map.get(&anchor.ev) else {
            continue;
        };
        let key = (
            ev.file.clone(),
            ev.start_line,
            ev.end_line,
            ev.source_hash.clone(),
        );
        if !seen.insert(key) {
            continue;
        }
        items.push(EvidencePointer {
            file: ev.file.clone(),
            start_line: ev.start_line,
            end_line: ev.end_line,
            source_hash: ev.source_hash.clone(),
        });
    }

    if items.len() < 2 {
        let mut steps = parse_cpv1_steps(pack, &dict);
        steps.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.label.cmp(&b.label)));
        for step in steps {
            if items.len() >= 3 {
                break;
            }
            let Some(ev) = ev_map.get(&step.ev) else {
                continue;
            };
            let key = (
                ev.file.clone(),
                ev.start_line,
                ev.end_line,
                ev.source_hash.clone(),
            );
            if !seen.insert(key) {
                continue;
            }
            items.push(EvidencePointer {
                file: ev.file.clone(),
                start_line: ev.start_line,
                end_line: ev.end_line,
                source_hash: ev.source_hash.clone(),
            });
        }
    }

    items
}

struct NextActionHints<'a> {
    best_worktree_path: Option<&'a str>,
    worktrees_truncated: bool,
    worktrees_next_cursor: Option<&'a str>,
    worktrees_len: usize,
    notebook_empty: bool,
}

fn derive_next_actions(
    root_path: &str,
    query: &str,
    pack: &str,
    hints: NextActionHints<'_>,
) -> Vec<ToolNextAction> {
    let mut actions: Vec<ToolNextAction> = Vec::new();
    let dict = parse_cpv1_dict(pack);

    if let Some(focus) = pick_focus_from_pack(pack, &dict) {
        actions.push(ToolNextAction {
            tool: "meaning_focus".to_string(),
            args: json!({
                "path": root_path,
                "focus": focus,
                "query": query,
                "max_chars": 2000,
                "response_mode": "full",
            }),
            reason: "Zoom into the most evidence-dense area before reading".to_string(),
        });
    }

    let items = derive_evidence_items(pack);
    if !items.is_empty() {
        actions.push(ToolNextAction {
            tool: "evidence_fetch".to_string(),
            args: json!({
                "path": root_path,
                "items": items,
                "max_chars": 2000,
                "max_lines": 200,
                "response_mode": "facts",
            }),
            reason: "Fetch verbatim evidence for canon/CI/contracts/entrypoints".to_string(),
        });
    }

    if let Some(worktree_path) = hints.best_worktree_path {
        if worktree_path != root_path {
            actions.push(ToolNextAction {
                tool: "meaning_pack".to_string(),
                args: json!({
                    "path": worktree_path,
                    "query": query,
                    "max_chars": 2000,
                    "response_mode": "full",
                }),
                reason: "Build a meaning map for the most relevant worktree".to_string(),
            });
        }
    }

    if hints.worktrees_truncated || hints.worktrees_len > 1 {
        let mut args = json!({
            "path": root_path,
            "response_mode": "full",
            "max_chars": 2000,
        });
        if let Some(cursor) = hints.worktrees_next_cursor {
            if let Some(obj) = args.as_object_mut() {
                obj.insert("cursor".to_string(), json!(cursor));
            }
        }
        actions.push(ToolNextAction {
            tool: "worktree_pack".to_string(),
            args,
            reason:
                "Drill into worktrees/branches (purpose summaries are evidence-backed in full mode)"
                    .to_string(),
        });
    }

    if hints.notebook_empty {
        actions.push(ToolNextAction {
            tool: "notebook_suggest".to_string(),
            args: json!({
                "path": root_path,
                "query": query,
                "max_chars": 2000,
                "response_mode": "full",
            }),
            reason: "Generate durable anchors + runbooks for cross-session continuity.".to_string(),
        });
    }

    actions
}

pub(super) async fn compute_atlas_pack_result(
    root: &Path,
    root_display: &str,
    request: &AtlasPackRequest,
) -> Result<AtlasPackResult> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let max_chars = clamp_max_chars(request.max_chars);
    let query =
        normalize_query(request.query.as_deref()).unwrap_or_else(|| DEFAULT_QUERY.to_string());
    let worktree_limit = clamp_worktree_limit(request.worktree_limit);
    let root_path = root.to_string_lossy().to_string();

    // Budget split: prefer meaning pack first; include worktrees only if we can afford the
    // worktree_pack minimum (it clamps to >=800 chars internally).
    let reserve_overhead = 280usize;
    let content_budget = max_chars.saturating_sub(reserve_overhead).max(1);
    let mut meaning_max_chars = (content_budget * 2 / 3).max(900).min(content_budget);
    let mut worktree_max_chars = content_budget.saturating_sub(meaning_max_chars);
    let include_worktrees = worktree_max_chars >= 800;
    if !include_worktrees {
        meaning_max_chars = content_budget;
        worktree_max_chars = 0;
    }

    let meaning_request = meaning::MeaningPackRequest {
        query: query.clone(),
        map_depth: Some(2),
        map_limit: None,
        max_chars: Some(meaning_max_chars),
    };
    let meaning_result = meaning::meaning_pack(root, root_display, &meaning_request)
        .await
        .with_context(|| "build meaning pack")?;

    let mut worktrees: Vec<super::schemas::worktree_pack::WorktreeInfo> = Vec::new();
    let mut worktrees_truncated = false;
    let mut worktrees_next_cursor: Option<String> = None;

    if include_worktrees {
        let wt_request = WorktreePackRequest {
            path: None,
            query: Some(query.clone()),
            limit: Some(worktree_limit),
            max_chars: Some(worktree_max_chars),
            response_mode: Some(response_mode),
            cursor: None,
        };
        let wt = compute_worktree_pack_result(root, root_display, &wt_request, None)
            .await
            .with_context(|| "compute worktree summary")?;
        worktrees = wt.worktrees;
        worktrees_truncated = wt.truncated;
        worktrees_next_cursor = wt.next_cursor;
    }

    let next_actions = if response_mode == ResponseMode::Full {
        let notebook_empty = {
            let identity = resolve_repo_identity(root);
            let paths = notebook_paths_for_scope(root, NotebookScope::Project, &identity)?;
            let notebook = load_or_init_notebook(root, &paths)?;
            notebook.anchors.is_empty() && notebook.runbooks.is_empty()
        };
        let hints = NextActionHints {
            best_worktree_path: worktrees.first().map(|w| w.path.as_str()),
            worktrees_truncated: worktrees_truncated || !include_worktrees,
            worktrees_next_cursor: worktrees_next_cursor.as_deref(),
            worktrees_len: worktrees.len(),
            notebook_empty,
        };
        let actions = derive_next_actions(&root_path, &query, &meaning_result.pack, hints);
        if actions.is_empty() {
            None
        } else {
            Some(actions)
        }
    } else {
        None
    };

    Ok(AtlasPackResult {
        version: VERSION,
        query,
        meaning_format: meaning_result.format,
        meaning_max_chars: meaning_result.budget.max_chars,
        meaning_used_chars: meaning_result.budget.used_chars,
        meaning_truncated: meaning_result.budget.truncated,
        meaning_truncation: meaning_result.budget.truncation,
        meaning_pack: meaning_result.pack,
        worktrees,
        worktrees_truncated: worktrees_truncated || !include_worktrees,
        worktrees_next_cursor,
        next_actions,
        budget: AtlasPackBudget {
            max_chars,
            used_chars: 0,
            truncated: meaning_result.budget.truncated || worktrees_truncated || !include_worktrees,
            truncation: None,
        },
        meta: Some(ToolMeta::default()),
    })
}
