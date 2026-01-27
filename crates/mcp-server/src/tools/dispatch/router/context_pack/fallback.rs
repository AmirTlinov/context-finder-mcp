use super::super::super::{
    current_model_id, mcp_default_budgets, CallToolResult, Content, ContextFinderService,
    ResponseMode, CONTEXT_PACK_VERSION,
};
use super::super::error::{internal_error_with_meta, invalid_request};
use super::super::semantic_fallback::grep_fallback_hunks;
use super::budget::enforce_context_pack_budget;
use super::inputs::ContextPackInputs;
use crate::tools::context_doc::ContextDocBuilder;
use context_protocol::{BudgetTruncation, ToolNextAction};
use context_search::{ContextPackBudget, ContextPackItem, ContextPackOutput};
use std::path::Path;

pub(super) fn choose_fallback_token(tokens: &[String]) -> Option<String> {
    fn is_low_value(token_lc: &str) -> bool {
        matches!(
            token_lc,
            "struct"
                | "definition"
                | "define"
                | "defined"
                | "fn"
                | "function"
                | "method"
                | "class"
                | "type"
                | "enum"
                | "trait"
                | "impl"
                | "module"
                | "file"
                | "path"
                | "usage"
                | "usages"
                | "reference"
                | "references"
                | "what"
                | "where"
                | "find"
                | "show"
        )
    }

    let mut best: Option<String> = None;
    for token in tokens {
        let token = token.trim();
        if token.len() < 4 {
            continue;
        }
        let token_lc = token.to_lowercase();
        if is_low_value(&token_lc) {
            continue;
        }
        let looks_like_identifier = token
            .chars()
            .any(|ch| ch.is_ascii_uppercase() || ch == '_' || ch == '-');
        if !looks_like_identifier && token.len() < 8 {
            continue;
        }
        if best.as_ref().is_none_or(|b| token.len() > b.len()) {
            best = Some(token.to_string());
        }
    }

    best.or_else(|| {
        tokens
            .iter()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .max_by_key(|t| t.len())
            .map(|t| t.to_string())
    })
}

pub(super) fn items_mention_token(items: &[ContextPackItem], token: &str) -> bool {
    let token = token.trim();
    if token.is_empty() {
        return true;
    }
    let token_lc = token.to_lowercase();
    items.iter().take(6).any(|item| {
        item.symbol
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(token))
            || item.file.contains(token)
            || item.file.to_lowercase().contains(&token_lc)
            || item.content.contains(token)
            || item.content.to_lowercase().contains(&token_lc)
    })
}

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

    let hunks = grep_fallback_hunks(
        root,
        root_display,
        args.fallback_pattern,
        inputs.response_mode,
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

    for (idx, hunk) in hunks.into_iter().enumerate() {
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
    if inputs.response_mode == ResponseMode::Full {
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

    if inputs.response_mode == ResponseMode::Minimal {
        args.meta.index_state = None;
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

    enforce_context_pack_budget(&mut output)?;

    let mut doc = ContextDocBuilder::new();
    let answer = if inputs.response_mode == ResponseMode::Full {
        format!("context_pack: {} items (fallback)", output.items.len())
    } else {
        format!("context_pack: {} items", output.items.len())
    };
    doc.push_answer(&answer);
    if inputs.response_mode != ResponseMode::Minimal {
        doc.push_root_fingerprint(output.meta.root_fingerprint);
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
