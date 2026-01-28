use super::super::super::{
    current_model_id, mcp_default_budgets, CallToolResult, Content, ContextFinderService,
    ResponseMode, CONTEXT_PACK_VERSION,
};
use super::super::error::{internal_error_with_meta, invalid_request};
use super::budget::enforce_context_pack_budget;
use super::inputs::ContextPackInputs;
use super::render;
use crate::tools::context_doc::ContextDocBuilder;
use context_protocol::{BudgetTruncation, ToolNextAction};
use context_search::{
    count_anchor_hits, ContextPackBudget, ContextPackItem, ContextPackOutput, DetectedAnchor,
};
use std::path::Path;

pub(super) use super::fallback_helpers::{choose_fallback_token, items_mention_token};

pub(super) struct LexicalFallbackArgs<'a> {
    pub(super) query: &'a str,
    pub(super) fallback_pattern: &'a str,
    pub(super) meta: context_indexer::ToolMeta,
    pub(super) reason_note: Option<&'a str>,
}

pub(super) async fn build_lexical_fallback_result(
    service: &ContextFinderService,
    root: &Path,
    root_display: &str,
    inputs: &ContextPackInputs,
    mut args: LexicalFallbackArgs<'_>,
) -> super::ToolResult<CallToolResult> {
    let budgets = mcp_default_budgets();
    let fallback_max_chars = inputs.max_chars.min(budgets.grep_context_max_chars);
    let max_hunks = inputs.limit.min(10);
    let all_hunks = super::fallback_helpers::collect_scoped_fallback_hunks(
        root,
        root_display,
        args.fallback_pattern,
        inputs,
        max_hunks,
        fallback_max_chars,
    )
    .await
    .map_err(|err| invalid_request(format!("Error: lexical fallback grep failed ({err:#})")))?;

    let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    let profile = service.profile.name().to_string();

    let mut items = Vec::new();
    let mut used_chars = 0usize;
    let mut dropped_items = 0usize;

    for (idx, hunk) in all_hunks.into_iter().enumerate() {
        if items.len() >= inputs.limit {
            dropped_items += 1;
            continue;
        }

        let content_chars = hunk.content.chars().count();
        if used_chars.saturating_add(content_chars) > inputs.max_chars {
            dropped_items += 1;
            continue;
        }
        used_chars = used_chars.saturating_add(content_chars);

        items.push(ContextPackItem {
            id: format!("lexical:{}:{}:{}", hunk.file, hunk.start_line, idx),
            role: "primary".to_string(),
            file: hunk.file,
            start_line: hunk.start_line,
            end_line: hunk.end_line,
            symbol: None,
            chunk_type: None,
            score: (1.0 - idx as f32 * 0.01).max(0.0),
            imports: Vec::new(),
            content: hunk.content,
            relationship: None,
            distance: None,
        });
    }

    let truncated = dropped_items > 0;
    let budget = ContextPackBudget {
        max_chars: inputs.max_chars,
        used_chars,
        truncated,
        dropped_items,
        truncation: truncated.then_some(BudgetTruncation::MaxChars),
    };

    let mut next_actions = Vec::new();
    let include_next_actions =
        inputs.format_version == 2 || inputs.response_mode != ResponseMode::Minimal;
    if include_next_actions {
        if let Some(top) = items.first() {
            next_actions.push(ToolNextAction {
                tool: "file_slice".to_string(),
                args: serde_json::json!({
                    "path": root_display,
                    "file": top.file.clone(),
                    "start_line": top.start_line.saturating_sub(40).max(1),
                    "max_lines": 200,
                    "format": "numbered",
                    "response_mode": "facts"
                }),
                reason: "Open the top hunk as verbatim code (territory) via file_slice."
                    .to_string(),
            });
        }
        next_actions.push(ToolNextAction {
            tool: "text_search".to_string(),
            args: serde_json::json!({
                "path": root_display,
                "pattern": args.fallback_pattern,
                "max_results": 80,
                "case_sensitive": false,
                "whole_word": true,
                "response_mode": "facts"
            }),
            reason: "Verify the exact anchor term via text_search (helps detect wrong root, typos, or stale index).".to_string(),
        });
        next_actions.push(ToolNextAction {
            tool: "repo_onboarding_pack".to_string(),
            args: serde_json::json!({
                "path": root_display,
                "max_chars": 12000,
                "response_mode": "facts"
            }),
            reason: "If results still look wrong, re-onboard the repo to confirm the effective root and key docs.".to_string(),
        });
    }

    if inputs.response_mode == ResponseMode::Minimal && inputs.format_version != 2 {
        args.meta.index_state = None;
        args.meta.trust = None;
    }

    let mut output = ContextPackOutput {
        version: CONTEXT_PACK_VERSION,
        query: args.query.to_string(),
        model_id,
        profile,
        items,
        budget,
        next_actions,
        meta: args.meta,
    };

    if let Some(trust) = output.meta.trust.as_mut() {
        trust.retrieval_mode = Some(context_indexer::RetrievalMode::Lexical);
        trust.fallback_used = Some(true);
        if trust.anchor_detected.unwrap_or(false) {
            if let (Some(kind), Some(primary)) =
                (trust.anchor_kind, trust.anchor_primary.as_deref())
            {
                let anchor = DetectedAnchor {
                    kind,
                    raw: primary.to_string(),
                    normalized: primary.to_string(),
                };
                let hits = count_anchor_hits(&output.items, &anchor);
                trust.anchor_hits = Some(hits);
                trust.anchor_not_found = Some(hits == 0);
            }
        }
    }

    enforce_context_pack_budget(&mut output)?;

    let mut doc = ContextDocBuilder::new();
    let answer = if inputs.response_mode == ResponseMode::Full {
        format!("context_pack: {} items (fallback)", output.items.len())
    } else {
        format!("context_pack: {} items", output.items.len())
    };
    doc.push_answer(&answer);
    if inputs.format_version == 2 {
        render::push_v2_envelope(&mut doc, &output, None);
        render::push_next_actions_v2(&mut doc, &output);
    } else {
        if inputs.response_mode != ResponseMode::Minimal {
            doc.push_root_fingerprint(output.meta.root_fingerprint);
        }
        render::maybe_push_trust_micro_meta(&mut doc, inputs.response_mode, &output, None);
        if inputs.response_mode != ResponseMode::Minimal {
            render::push_next_actions(&mut doc, &output);
        }
    }
    if inputs.response_mode == ResponseMode::Full {
        if let Some(note) = args.reason_note {
            doc.push_note(note);
        }
        doc.push_note(&format!("fallback_pattern: {}", args.fallback_pattern));
    }
    if output.items.is_empty() && inputs.response_mode != ResponseMode::Minimal {
        if inputs.response_mode == ResponseMode::Full {
            doc.push_note("no matches found for fallback pattern");
        } else {
            doc.push_note("no matches found");
        }
    }
    for item in &output.items {
        doc.push_ref_header(&item.file, item.start_line, item.symbol.as_deref());
        doc.push_block_smart(&item.content);
        doc.push_blank();
    }

    let (rendered, envelope_truncated) = doc.finish_bounded(output.budget.max_chars);
    if envelope_truncated {
        output.budget.truncated = true;
        if output.budget.truncation.is_none() {
            output.budget.truncation = Some(BudgetTruncation::MaxChars);
        }
    }
    let mut result = CallToolResult::success(vec![Content::text(rendered)]);
    let structured = serde_json::to_value(&output).map_err(|err| {
        internal_error_with_meta(
            format!("Error: failed to serialize context_pack output ({err})"),
            output.meta.clone(),
        )
    })?;
    result.structured_content = Some(structured);
    Ok(result)
}
