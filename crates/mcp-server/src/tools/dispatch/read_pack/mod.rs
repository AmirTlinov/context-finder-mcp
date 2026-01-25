pub(super) use super::router::error::invalid_cursor_with_meta_details;
use super::router::error::{attach_meta, attach_structured_content, tool_error};
use super::{
    encode_cursor, finalize_read_pack_budget, CallToolResult, Content, ContextFinderService,
    McpError, ReadPackBudget, ReadPackIntent, ReadPackNextAction, ReadPackRequest, ReadPackResult,
    ReadPackSection, ReadPackTruncation, ResponseMode, CURSOR_VERSION,
};
pub(super) use super::{
    ProjectFactsResult, ReadPackRecallResult, ReadPackSnippet, ReadPackSnippetKind,
};
use std::time::Duration;

mod context;
pub(crate) use context::ReadPackContext;

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
mod prepare;
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

pub(super) use super::decode_cursor;
pub(super) use cursors::trimmed_non_empty_str;

use budget_trim::{finalize_and_trim, trim_project_facts_for_budget};
use cursor_repair::repair_cursor_after_trim;
use intent_file::handle_file_intent;
use intent_grep::handle_grep_intent;
use intent_memory::handle_memory_intent;
use intent_onboarding::handle_onboarding_intent;
use intent_query::{handle_query_intent, QueryIntentPolicy};
use intent_recall::handle_recall_intent;
use overlap::{overlap_dedupe_snippet_sections, strip_snippet_reasons_for_output};
use prepare::{prepare_read_pack, PreparedReadPack};
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
    request: ReadPackRequest,
) -> Result<CallToolResult, McpError> {
    let PreparedReadPack {
        request,
        ctx,
        intent,
        response_mode,
        timeout_ms,
        meta,
        meta_for_output,
        semantic_index_fresh,
        allow_secrets,
    } = match prepare_read_pack(service, request).await {
        Ok(prepared) => prepared,
        Err(result) => return Ok(result),
    };

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
