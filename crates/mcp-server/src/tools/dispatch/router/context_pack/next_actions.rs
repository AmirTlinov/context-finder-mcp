use super::super::inputs::ContextPackInputs;
use super::super::{fallback, render};
use crate::tools::dispatch::{mcp_default_budgets, ResponseMode};
use crate::tools::schemas::ToolNextAction;
use context_search::{count_anchor_hits, detect_primary_anchor, ContextPackOutput};

fn file_slice_action(root_display: &str, file: &str, start_line: usize) -> ToolNextAction {
    ToolNextAction {
        tool: "file_slice".to_string(),
        args: serde_json::json!({
            "path": root_display,
            "file": file,
            "start_line": start_line.saturating_sub(40).max(1),
            "max_lines": 200,
            "format": "numbered",
            "response_mode": "facts"
        }),
        reason: "Open the top hit as verbatim code (territory) via file_slice.".to_string(),
    }
}

fn repo_onboarding_pack_action(root_display: &str) -> ToolNextAction {
    ToolNextAction {
        tool: "repo_onboarding_pack".to_string(),
        args: serde_json::json!({
            "path": root_display,
            "max_chars": 12000,
            "response_mode": "facts"
        }),
        reason: "No hits: re-onboard the repo to confirm the effective root and key docs."
            .to_string(),
    }
}

fn text_search_action(root_display: &str, pattern: &str) -> ToolNextAction {
    ToolNextAction {
        tool: "text_search".to_string(),
        args: serde_json::json!({
            "path": root_display,
            "pattern": pattern,
            "max_results": 80,
            "case_sensitive": false,
            "whole_word": true,
            "response_mode": "facts"
        }),
        reason: "Anchor guardrail: verify the exact anchor term via text_search (typos/wrong root/stale index)."
            .to_string(),
    }
}

pub(super) fn maybe_prepend_v2_file_slice(
    inputs: &ContextPackInputs,
    root_display: &str,
    output: &mut ContextPackOutput,
) {
    if inputs.format_version != 2 {
        return;
    }
    let Some(top) = output.items.first() else {
        return;
    };
    output.next_actions.insert(
        0,
        file_slice_action(root_display, &top.file, top.start_line),
    );
}

pub(super) fn maybe_cap_next_actions_for_v2(
    inputs: &ContextPackInputs,
    output: &mut ContextPackOutput,
) {
    if inputs.format_version != 2 {
        return;
    }
    if output.next_actions.len() > 3 {
        output.next_actions.truncate(3);
    }
}

pub(super) fn maybe_add_low_noise_next_actions(
    inputs: &ContextPackInputs,
    root_display: &str,
    query: &str,
    output: &mut ContextPackOutput,
    semantic_disabled_reason: Option<&str>,
) {
    let primary_anchor = detect_primary_anchor(&output.query);
    let anchor_detected = primary_anchor.is_some();
    let anchor_hits = primary_anchor
        .as_ref()
        .map(|a| count_anchor_hits(&output.items, a))
        .unwrap_or(0);
    let index_anomaly = output
        .meta
        .index_state
        .as_ref()
        .map(|s| !s.index.exists || s.stale)
        .unwrap_or(false);
    let fallback_used = output
        .meta
        .trust
        .as_ref()
        .and_then(|t| t.fallback_used)
        .unwrap_or(false)
        || semantic_disabled_reason.is_some()
        || output
            .items
            .iter()
            .any(|item| item.id.starts_with("lexical:"));
    let anomaly = output.items.is_empty()
        || output.budget.truncated
        || semantic_disabled_reason.is_some()
        || fallback_used
        || (anchor_detected && anchor_hits == 0)
        || index_anomaly;

    let want_next_actions = match inputs.response_mode {
        ResponseMode::Full => true,
        ResponseMode::Facts => inputs.format_version == 2 || anomaly,
        ResponseMode::Minimal => inputs.format_version == 2,
    };
    if !want_next_actions {
        return;
    }

    if let Some(top) = output.items.first() {
        output
            .next_actions
            .push(file_slice_action(root_display, &top.file, top.start_line));
    }
    if anchor_detected && anchor_hits == 0 {
        if let Some(anchor) = primary_anchor.as_ref() {
            output
                .next_actions
                .push(text_search_action(root_display, &anchor.normalized));
        }
    }
    if output.budget.truncated {
        output.next_actions.push(render::build_retry_action(
            root_display,
            query,
            inputs,
            output,
        ));
    }
    if output.items.is_empty() {
        output
            .next_actions
            .push(repo_onboarding_pack_action(root_display));
    }

    if inputs.response_mode != ResponseMode::Full && output.next_actions.len() > 3 {
        output.next_actions.truncate(3);
    }
}

pub(super) fn add_full_mode_next_actions(
    inputs: &ContextPackInputs,
    root_display: &str,
    query: &str,
    output: &mut ContextPackOutput,
    semantic_disabled_reason: Option<&str>,
) {
    maybe_prepend_v2_file_slice(inputs, root_display, output);

    if output.items.is_empty() && semantic_disabled_reason.is_some() {
        let budgets = mcp_default_budgets();
        let pattern = inputs
            .query_tokens
            .iter()
            .max_by_key(|t| t.len())
            .cloned()
            .unwrap_or_else(|| query.trim().to_string());
        output.next_actions.push(ToolNextAction {
            tool: "rg".to_string(),
            args: serde_json::json!({
                "path": root_display,
                "pattern": pattern,
                "literal": true,
                "case_sensitive": false,
                "context": 2,
                "max_chars": budgets.grep_context_max_chars,
                "max_hunks": 8,
                "format": "numbered",
                "response_mode": "facts"
            }),
            reason:
                "Semantic search is disabled; fall back to rg on the most relevant query token."
                    .to_string(),
        });
    }

    if output.items.is_empty() {
        let pattern = fallback::choose_fallback_token(&inputs.query_tokens)
            .or_else(|| inputs.query_tokens.iter().max_by_key(|t| t.len()).cloned())
            .unwrap_or_else(|| query.trim().to_string());
        output
            .next_actions
            .push(text_search_action(root_display, &pattern));
        output
            .next_actions
            .push(repo_onboarding_pack_action(root_display));
        return;
    }

    if let Some(token) = fallback::choose_fallback_token(&inputs.query_tokens) {
        if !fallback::items_mention_token(&output.items, &token) {
            output
                .next_actions
                .push(text_search_action(root_display, &token));
        }
    }
}
