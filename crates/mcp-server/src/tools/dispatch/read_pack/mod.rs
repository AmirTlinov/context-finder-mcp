use super::router::cursor_alias::expand_cursor_alias;
pub(super) use super::router::error::invalid_cursor_with_meta_details;
use super::router::error::{
    attach_meta, attach_structured_content, invalid_request_with_root_context, meta_for_request,
    tool_error,
};
use super::{
    decode_cursor, encode_cursor, finalize_read_pack_budget, AutoIndexPolicy, CallToolResult,
    Content, ContextFinderService, McpError, ReadPackBudget, ReadPackIntent, ReadPackNextAction,
    ReadPackRequest, ReadPackResult, ReadPackSection, ReadPackTruncation, ResponseMode,
    CURSOR_VERSION,
};
pub(super) use super::{
    ProjectFactsResult, ReadPackRecallResult, ReadPackSnippet, ReadPackSnippetKind,
};
use context_indexer::{root_fingerprint, ToolMeta};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

mod context;
use context::{build_context, ReadPackContext};

mod anchor_scan;
mod budget_trim;
mod candidates;
mod cursor_repair;
mod cursors;
mod file_cursor;
mod file_limits;
mod fs_scan;
mod grep_cursor;
mod intent_file;
mod intent_grep;
mod intent_memory;
mod intent_onboarding;
mod intent_query;
mod intent_recall;
mod intent_resolve;
mod memory_cursor;
mod memory_overview;
mod memory_snippets;
mod onboarding_command;
mod onboarding_docs;
mod onboarding_topics;
mod overlap;
mod project_facts;
mod recall;
mod recall_cursor;
mod recall_directives;
mod recall_keywords;
mod recall_ops;
mod recall_paths;
mod recall_scoring;
mod recall_snippets;
mod recall_structural;
mod recall_trim;
mod render;
mod retry;
mod session;

use budget_trim::{finalize_and_trim, trim_project_facts_for_budget};
use cursor_repair::repair_cursor_after_trim;
use cursors::trimmed_non_empty_str;
use intent_file::handle_file_intent;
use intent_grep::handle_grep_intent;
use intent_memory::handle_memory_intent;
use intent_onboarding::handle_onboarding_intent;
use intent_query::{handle_query_intent, QueryIntentPolicy};
use intent_recall::handle_recall_intent;
use intent_resolve::resolve_intent;
use overlap::{overlap_dedupe_snippet_sections, strip_snippet_reasons_for_output};
use project_facts::compute_project_facts;
use render::{
    apply_meta_to_sections, entrypoint_candidate_score, render_read_pack_context_doc,
    truncate_to_chars,
};
use session::note_session_working_set_from_read_pack_result;

const VERSION: u32 = 1;
const DEFAULT_MAX_CHARS: usize = 6_000;
const MIN_MAX_CHARS: usize = 400;
const MAX_MAX_CHARS: usize = 500_000;
const DEFAULT_GREP_CONTEXT: usize = 20;
const MAX_GREP_MATCHES: usize = 10_000;
const MAX_GREP_HUNKS: usize = 200;
// Agent-native default: keep tool calls snappy so the agent can stay in a tight loop.
// Callers can always opt in to longer work via `timeout_ms` (and/or `deep` for recall).
const DEFAULT_TIMEOUT_MS: u64 = 12_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const MAX_RECALL_INLINE_CURSOR_CHARS: usize = 1_200;

type ToolResult<T> = std::result::Result<T, CallToolResult>;

fn call_error(code: &'static str, message: impl Into<String>) -> CallToolResult {
    tool_error(code, message)
}

const REASON_ANCHOR_FOCUS_FILE: &str = "anchor:focus_file";
const REASON_ANCHOR_DOC: &str = "anchor:doc";
const REASON_ANCHOR_ENTRYPOINT: &str = "anchor:entrypoint";
const REASON_NEEDLE_GREP_HUNK: &str = "needle:grep_hunk";
const REASON_NEEDLE_FILE_SLICE: &str = "needle:cat";
const REASON_HALO_CONTEXT_PACK_PRIMARY: &str = "halo:context_pack_primary";
const REASON_HALO_CONTEXT_PACK_RELATED: &str = "halo:context_pack_related";
const REASON_INTENT_FILE: &str = "intent:file";

/// Build a one-call semantic reading pack (file slice / grep context / context pack / onboarding).
pub(in crate::tools::dispatch) async fn read_pack(
    service: &ContextFinderService,
    mut request: ReadPackRequest,
) -> Result<CallToolResult, McpError> {
    // Expand compact cursor aliases early so routing and cursor-only continuation work.
    // Without this, `resolve_intent` would attempt to decode a non-base64 cursor alias directly.
    if let Some(cursor) = request.cursor.as_deref() {
        match expand_cursor_alias(service, cursor).await {
            Ok(expanded) => request.cursor = Some(expanded),
            Err(message) => {
                let meta = meta_for_request(service, request.path.as_deref()).await;
                return Ok(attach_meta(call_error("invalid_cursor", message), meta));
            }
        }
    }

    // Cursor-only continuation: if the caller didn't pass `path`, we can fall back to the cursor's
    // embedded root *only when the current session has no established root*.
    //
    // This is a safety boundary for multi-agent / multi-project usage: if a session already has a
    // default root (from a previous call), we refuse to silently switch projects based on a cursor
    // token that might have been copy/pasted or mixed across concurrent agent sessions.
    if trimmed_non_empty_str(request.path.as_deref()).is_none() {
        if let Some(cursor) = request.cursor.as_deref() {
            if let Ok(value) = decode_cursor::<Value>(cursor) {
                if let Some(root) = value.get("root").and_then(Value::as_str) {
                    let cursor_root = root.trim();
                    if !cursor_root.is_empty() {
                        let session_root_display = { service.session.lock().await.root_display() };
                        if let Some(session_root_display) = session_root_display {
                            if session_root_display != cursor_root {
                                let message = "Invalid cursor: cursor refers to a different project root than the current session; call `root_set` to switch projects (or pass `path`)."
                                    .to_string();
                                let meta = ToolMeta {
                                    root_fingerprint: Some(root_fingerprint(&session_root_display)),
                                    ..ToolMeta::default()
                                };
                                return Ok(attach_meta(
                                    call_error("invalid_cursor", message),
                                    meta,
                                ));
                            }
                        } else {
                            request.path = Some(cursor_root.to_string());
                        }
                    }
                }
            }
        }
    }

    // DX convenience: callers often pass `path` as a *subdirectory or file within the project*
    // (e.g. `{ \"path\": \"src\", \"pattern\": \"foo\" }` or `{ \"path\": \"README.md\" }`).
    // In Context, `path` sets the project root.
    //
    // When the session already has a root, treat a relative `path` with no `file`/`file_pattern`
    // and no cursor as a file/file_pattern hint instead of switching the session root.
    let cursor_missing = trimmed_non_empty_str(request.cursor.as_deref()).is_none();
    let file_missing = trimmed_non_empty_str(request.file.as_deref()).is_none();
    let file_pattern_missing = trimmed_non_empty_str(request.file_pattern.as_deref()).is_none();
    if cursor_missing && file_missing && file_pattern_missing {
        if let Some(raw_path) = trimmed_non_empty_str(request.path.as_deref()) {
            let session_root = { service.session.lock().await.clone_root().map(|(r, _)| r) };
            if let Some(session_root) = session_root.as_ref() {
                let raw = Path::new(raw_path);
                if raw.is_absolute() {
                    if let Ok(canonical) = raw.canonicalize() {
                        if canonical.starts_with(session_root) {
                            if let Ok(rel) = canonical.strip_prefix(session_root) {
                                if let Some(rel) =
                                    crate::tools::dispatch::root::rel_path_string(rel)
                                {
                                    let is_file = std::fs::metadata(&canonical)
                                        .ok()
                                        .map(|meta| meta.is_file())
                                        .unwrap_or(false);
                                    if is_file {
                                        request.file = Some(rel);
                                    } else {
                                        let mut pattern = rel;
                                        if !pattern.ends_with('/') {
                                            pattern.push('/');
                                        }
                                        request.file_pattern = Some(pattern);
                                    }
                                    request.path = None;
                                }
                            }
                        }
                    }
                } else {
                    let normalized = raw_path.trim_start_matches("./");
                    if normalized == "." || normalized.is_empty() {
                        request.path = None;
                    } else {
                        let candidate = session_root.join(normalized);
                        let meta = std::fs::metadata(&candidate).ok();
                        let is_file = meta.as_ref().map(|m| m.is_file()).unwrap_or(false);
                        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                        if is_file {
                            request.file = Some(normalized.to_string());
                        } else {
                            let mut pattern = normalized.to_string();
                            if is_dir && !pattern.ends_with('/') {
                                pattern.push('/');
                            }
                            request.file_pattern = Some(pattern);
                        }
                        request.path = None;
                    }
                }
            }
        }
    }

    let mut hints: Vec<String> = Vec::new();
    if let Some(file) = request.file.as_deref() {
        hints.push(file.to_string());
    }
    if let Some(pattern) = request.file_pattern.as_deref() {
        if !pattern.trim().is_empty() {
            hints.push(pattern.to_string());
        }
    }
    let (root, root_display) = match service
        .resolve_root_with_hints_for_tool(request.path.as_deref(), &hints, "read_pack")
        .await
    {
        Ok(value) => value,
        Err(message) => {
            return Ok(invalid_request_with_root_context(
                service,
                message,
                ToolMeta::default(),
                None,
                Vec::new(),
            )
            .await)
        }
    };
    let base_meta = service.tool_meta(&root).await;

    // Cursor-only continuation should preserve caller-selected budgets and response mode.
    // Without this, a continuation can silently jump back to defaults (e.g. max_chars=20k), which
    // is catastrophic for an agent’s context window.
    if let Some(cursor) = request.cursor.as_deref() {
        match decode_cursor::<Value>(cursor) {
            Ok(value) => {
                if request.max_chars.is_none() {
                    if let Some(n) = value.get("max_chars").and_then(Value::as_u64) {
                        if n > 0 {
                            request.max_chars = Some(n as usize);
                        }
                    }
                }
                if request.response_mode.is_none() {
                    if let Some(mode_value) = value.get("response_mode") {
                        if let Ok(mode) = serde_json::from_value::<ResponseMode>(mode_value.clone())
                        {
                            request.response_mode = Some(mode);
                        }
                    }
                }
            }
            Err(err) => {
                return Ok(attach_meta(
                    call_error("invalid_cursor", format!("Invalid cursor: {err}")),
                    base_meta.clone(),
                ))
            }
        }
    }

    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let ctx = match build_context(&request, root, root_display) {
        Ok(value) => value,
        Err(result) => return Ok(attach_meta(result, base_meta.clone())),
    };
    let intent = match resolve_intent(&request) {
        Ok(value) => value,
        Err(result) => return Ok(attach_meta(result, base_meta.clone())),
    };

    let timeout_ms = request
        .timeout_ms
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(1_000, MAX_TIMEOUT_MS);
    let meta = match intent {
        ReadPackIntent::Query => {
            // Query intent is the "semantic default" surface: keep it fresh when possible.
            service
                .tool_meta_with_auto_index(&ctx.root, AutoIndexPolicy::semantic_default())
                .await
        }
        _ => base_meta.clone(),
    };
    // Low-noise default: keep the response mostly project content. Freshness/index diagnostics are
    // opt-in via `response_mode=full`.
    let provenance_meta = ToolMeta {
        root_fingerprint: meta.root_fingerprint,
        ..ToolMeta::default()
    };
    let meta_for_output = if response_mode == ResponseMode::Full {
        Some(meta.clone())
    } else {
        Some(provenance_meta)
    };

    let semantic_index_fresh = meta
        .index_state
        .as_ref()
        .is_some_and(|state| state.index.exists && !state.stale);
    let allow_secrets = request.allow_secrets.unwrap_or(false);

    let mut sections: Vec<ReadPackSection> = Vec::new();
    let mut next_actions: Vec<ReadPackNextAction> = Vec::new();
    let mut next_cursor: Option<String> = None;

    let facts = service
        .state
        .project_facts_cache_get(&ctx.root)
        .await
        .unwrap_or_else(|| compute_project_facts(&ctx.root));
    service
        .state
        .project_facts_cache_put(&ctx.root, facts.clone())
        .await;
    let facts = trim_project_facts_for_budget(facts, &ctx, response_mode);
    sections.push(ReadPackSection::ProjectFacts {
        result: facts.clone(),
    });

    let handler_future = async {
        match intent {
            ReadPackIntent::Auto => unreachable!("auto intent resolved above"),
            ReadPackIntent::File => {
                handle_file_intent(
                    service,
                    &ctx,
                    &request,
                    response_mode,
                    &mut sections,
                    &mut next_actions,
                    &mut next_cursor,
                )
                .await
            }
            ReadPackIntent::Grep => {
                handle_grep_intent(
                    service,
                    &ctx,
                    &request,
                    response_mode,
                    &mut sections,
                    &mut next_actions,
                    &mut next_cursor,
                )
                .await
            }
            ReadPackIntent::Query => {
                handle_query_intent(
                    service,
                    &ctx,
                    &request,
                    response_mode,
                    QueryIntentPolicy { allow_secrets },
                    &mut sections,
                )
                .await
            }
            ReadPackIntent::Onboarding => {
                handle_onboarding_intent(&ctx, &request, response_mode, &facts, &mut sections).await
            }
            ReadPackIntent::Memory => {
                handle_memory_intent(
                    service,
                    &ctx,
                    &request,
                    response_mode,
                    &mut sections,
                    &mut next_actions,
                    &mut next_cursor,
                )
                .await
            }
            ReadPackIntent::Recall => {
                handle_recall_intent(
                    service,
                    &ctx,
                    &request,
                    response_mode,
                    semantic_index_fresh,
                    &mut sections,
                    &mut next_cursor,
                )
                .await
            }
        }
    };
    let handler_result =
        match tokio::time::timeout(Duration::from_millis(timeout_ms), handler_future).await {
            Ok(result) => result,
            Err(_) => {
                let mut result = ReadPackResult {
                    version: VERSION,
                    intent,
                    root: ctx.root_display.clone(),
                    sections,
                    next_actions,
                    next_cursor,
                    budget: ReadPackBudget {
                        max_chars: ctx.max_chars,
                        used_chars: 0,
                        truncated: true,
                        truncation: Some(ReadPackTruncation::Timeout),
                    },
                    meta: meta_for_output.clone(),
                };
                overlap_dedupe_snippet_sections(&mut result.sections);
                if response_mode != ResponseMode::Full {
                    strip_snippet_reasons_for_output(&mut result.sections, true);
                }
                apply_meta_to_sections(&mut result.sections);
                let mut result =
                    match finalize_and_trim(result, &ctx, &request, intent, response_mode) {
                        Ok(value) => value,
                        Err(result) => return Ok(attach_meta(result, meta.clone())),
                    };
                repair_cursor_after_trim(
                    service,
                    &ctx,
                    &request,
                    intent,
                    response_mode,
                    &mut result,
                )
                .await;
                let _ = finalize_read_pack_budget(&mut result);
                while result.budget.used_chars > ctx.max_chars && result.sections.len() > 1 {
                    result.sections.pop();
                    result.next_actions.clear();
                    result.next_cursor = None;
                    repair_cursor_after_trim(
                        service,
                        &ctx,
                        &request,
                        intent,
                        response_mode,
                        &mut result,
                    )
                    .await;
                    let _ = finalize_read_pack_budget(&mut result);
                    result.budget.truncated = true;
                    if result.budget.truncation.is_none() {
                        result.budget.truncation = Some(ReadPackTruncation::MaxChars);
                    }
                }
                result.budget.truncated = true;
                result.budget.truncation = Some(ReadPackTruncation::Timeout);

                if response_mode != ResponseMode::Full {
                    strip_snippet_reasons_for_output(&mut result.sections, false);
                    let _ = finalize_read_pack_budget(&mut result);
                }

                note_session_working_set_from_read_pack_result(service, &result).await;

                let mut doc = render_read_pack_context_doc(&result, response_mode);
                loop {
                    if doc.chars().count() <= ctx.max_chars {
                        let output = CallToolResult::success(vec![Content::text(doc)]);
                        return Ok(attach_structured_content(
                            output,
                            &result,
                            result.meta.clone().unwrap_or_default(),
                            "read_pack",
                        ));
                    }
                    let cur_chars = doc.chars().count();
                    if cur_chars <= 1 {
                        break;
                    }
                    doc = truncate_to_chars(&doc, cur_chars.div_ceil(2));
                }
                let output = CallToolResult::success(vec![Content::text(doc)]);
                return Ok(attach_structured_content(
                    output,
                    &result,
                    result.meta.clone().unwrap_or_default(),
                    "read_pack",
                ));
            }
        };
    if let Err(result) = handler_result {
        return Ok(attach_meta(result, meta.clone()));
    }

    overlap_dedupe_snippet_sections(&mut sections);
    if response_mode != ResponseMode::Full {
        strip_snippet_reasons_for_output(&mut sections, true);
    }
    apply_meta_to_sections(&mut sections);
    let result = ReadPackResult {
        version: VERSION,
        intent,
        root: ctx.root_display.clone(),
        sections,
        next_actions,
        next_cursor,
        budget: ReadPackBudget {
            max_chars: ctx.max_chars,
            used_chars: 0,
            truncated: false,
            truncation: None,
        },
        meta: meta_for_output,
    };

    let result = match finalize_and_trim(result, &ctx, &request, intent, response_mode) {
        Ok(value) => value,
        Err(result) => return Ok(attach_meta(result, meta.clone())),
    };
    let mut result = result;
    repair_cursor_after_trim(service, &ctx, &request, intent, response_mode, &mut result).await;
    let _ = finalize_read_pack_budget(&mut result);
    while result.budget.used_chars > ctx.max_chars && result.sections.len() > 1 {
        result.sections.pop();
        result.next_actions.clear();
        result.next_cursor = None;
        repair_cursor_after_trim(service, &ctx, &request, intent, response_mode, &mut result).await;
        let _ = finalize_read_pack_budget(&mut result);
        result.budget.truncated = true;
        if result.budget.truncation.is_none() {
            result.budget.truncation = Some(ReadPackTruncation::MaxChars);
        }
    }

    if response_mode != ResponseMode::Full {
        strip_snippet_reasons_for_output(&mut result.sections, false);
        let _ = finalize_read_pack_budget(&mut result);
    }

    note_session_working_set_from_read_pack_result(service, &result).await;

    // `.context` output is returned as plain text (no JSON envelope).
    //
    // We still apply a defensive shrink loop because pack assembly may occasionally overshoot
    // under tiny budgets.
    let mut doc = render_read_pack_context_doc(&result, response_mode);
    loop {
        if doc.chars().count() <= ctx.max_chars {
            let output = CallToolResult::success(vec![Content::text(doc)]);
            return Ok(attach_structured_content(
                output,
                &result,
                result.meta.clone().unwrap_or_default(),
                "read_pack",
            ));
        }
        let cur_chars = doc.chars().count();
        if cur_chars <= 1 {
            break;
        }
        doc = truncate_to_chars(&doc, cur_chars.div_ceil(2));
    }
    let output = CallToolResult::success(vec![Content::text(doc)]);
    Ok(attach_structured_content(
        output,
        &result,
        result.meta.clone().unwrap_or_default(),
        "read_pack",
    ))
}

#[cfg(test)]
mod tests;
