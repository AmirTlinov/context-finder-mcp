use anyhow::Result;
use context_indexer::ToolMeta;
use context_protocol::{enforce_max_chars, finalize_used_chars};
use std::collections::HashSet;
use std::path::Path;

use super::file_slice::compute_onboarding_doc_slice;
use super::map::compute_map_result;
use super::schemas::repo_onboarding_pack::{
    RepoOnboardingDocsReason, RepoOnboardingNextAction, RepoOnboardingPackBudget,
    RepoOnboardingPackRequest, RepoOnboardingPackResult, RepoOnboardingPackTruncation,
};
use super::schemas::response_mode::ResponseMode;
use super::ContextFinderService;

const VERSION: u32 = 1;
const DEFAULT_MAX_CHARS: usize = 2_000;
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

pub(super) fn finalize_repo_onboarding_budget(
    result: &mut RepoOnboardingPackResult,
) -> anyhow::Result<()> {
    finalize_used_chars(result, |inner, used| inner.budget.used_chars = used).map(|_| ())
}

fn build_next_actions(root_display: &str, has_corpus: bool) -> Vec<RepoOnboardingNextAction> {
    let mut next_actions = Vec::new();
    if !has_corpus {
        next_actions.push(RepoOnboardingNextAction {
            tool: "search".to_string(),
            args: serde_json::json!({
                "path": root_display,
                "query": "what is the main entry point / architecture",
            }),
            reason: "Kick off semantic search; the index warms automatically (falls back to grep until ready).".to_string(),
        });
    }

    next_actions.push(RepoOnboardingNextAction {
        tool: "grep_context".to_string(),
        args: serde_json::json!({
            "path": root_display,
            "pattern": "TODO|FIXME",
            "context": 10,
            "max_hunks": 50,
        }),
        reason: "Scan for TODO/FIXME across the repo with surrounding context hunks.".to_string(),
    });

    next_actions.push(RepoOnboardingNextAction {
        tool: "batch".to_string(),
        args: serde_json::json!({
            "version": 2,
            "path": root_display,
            "max_chars": 20000,
            "items": [
                { "id": "docs", "tool": "list_files", "input": { "file_pattern": "*.md", "limit": 200 } },
                { "id": "read", "tool": "file_slice", "input": { "file": { "$ref": "#/items/docs/data/files/0" }, "start_line": 1, "max_lines": 200 } }
            ]
        }),
        reason: "Example: chain tools in one call with `$ref` dependencies (batch v2).".to_string(),
    });

    if has_corpus {
        next_actions.push(RepoOnboardingNextAction {
            tool: "context_pack".to_string(),
            args: serde_json::json!({
                "path": root_display,
                "query": "describe what you want to change",
                "strategy": "extended",
                "max_chars": 20000,
            }),
            reason: "One-shot semantic onboarding pack for a concrete question.".to_string(),
        });
    }

    next_actions
}

fn collect_doc_candidates(request: &RepoOnboardingPackRequest) -> Vec<String> {
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
    result: &mut RepoOnboardingPackResult,
    root: &Path,
    doc_candidates: &[String],
    docs_limit: usize,
    doc_max_lines: usize,
    doc_max_chars: usize,
) -> anyhow::Result<()> {
    for rel in doc_candidates {
        if result.docs.len() >= docs_limit {
            result.budget.truncated = true;
            result.budget.truncation = Some(RepoOnboardingPackTruncation::DocsLimit);
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
            result.budget.truncation = Some(RepoOnboardingPackTruncation::MaxChars);
            // Keep the current doc slice; map/next_actions can be trimmed to make room.
            break;
        }
    }

    Ok(())
}

fn trim_to_budget(result: &mut RepoOnboardingPackResult) -> anyhow::Result<()> {
    let max_chars = result.budget.max_chars;
    let reserved_docs = if result.docs.is_empty() { 0 } else { 1 };
    let enforce = |target: &mut RepoOnboardingPackResult, min_docs: usize| {
        enforce_max_chars(
            target,
            max_chars,
            |inner, used| inner.budget.used_chars = used,
            |inner| {
                inner.budget.truncated = true;
                inner.budget.truncation = Some(RepoOnboardingPackTruncation::MaxChars);
            },
            |inner| {
                if !inner.map.directories.is_empty() {
                    inner.map.directories.pop();
                    inner.map.truncated = true;
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

pub(super) async fn compute_repo_onboarding_pack_result(
    root: &Path,
    root_display: &str,
    request: &RepoOnboardingPackRequest,
) -> Result<RepoOnboardingPackResult> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let max_chars = request
        .max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);
    let map_depth = request.map_depth.unwrap_or(DEFAULT_MAP_DEPTH).clamp(1, 4);
    let map_limit = request.map_limit.unwrap_or(DEFAULT_MAP_LIMIT).clamp(1, 200);
    let docs_limit = request
        .docs_limit
        .unwrap_or(DEFAULT_DOCS_LIMIT)
        .clamp(0, MAX_DOCS_LIMIT);
    let doc_max_lines = request
        .doc_max_lines
        .unwrap_or(DEFAULT_DOC_MAX_LINES)
        .clamp(1, MAX_DOC_MAX_LINES);
    let doc_max_chars = request
        .doc_max_chars
        .unwrap_or(DEFAULT_DOC_MAX_CHARS)
        .clamp(1, MAX_DOC_MAX_CHARS);

    let map = compute_map_result(root, root_display, map_depth, map_limit, 0).await?;

    let has_corpus = ContextFinderService::load_chunk_corpus(root)
        .await
        .is_ok_and(|v| v.is_some());

    let next_actions = if response_mode == ResponseMode::Full {
        build_next_actions(root_display, has_corpus)
    } else {
        Vec::new()
    };
    let doc_candidates = collect_doc_candidates(request);

    let mut result = RepoOnboardingPackResult {
        version: VERSION,
        root: root_display.to_string(),
        map,
        docs: Vec::new(),
        docs_reason: None,
        next_actions,
        budget: RepoOnboardingPackBudget {
            max_chars,
            used_chars: 0,
            truncated: false,
            truncation: None,
        },
        meta: ToolMeta::default(),
    };

    add_docs_best_effort(
        &mut result,
        root,
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
        } else if matches!(
            result.budget.truncation,
            Some(RepoOnboardingPackTruncation::MaxChars)
        ) {
            RepoOnboardingDocsReason::MaxChars
        } else {
            RepoOnboardingDocsReason::DocsNotFound
        });
    }

    Ok(result)
}
