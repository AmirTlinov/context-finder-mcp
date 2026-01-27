use super::{budget, fallback, render, trace};
use crate::tools::schemas::ToolNextAction;

use super::super::super::{ContextFinderService, ResponseMode};
use super::inputs::ContextPackInputs;

pub(super) struct FinalizeContextPackArgs<'a> {
    pub(super) service: &'a ContextFinderService,
    pub(super) inputs: &'a ContextPackInputs,
    pub(super) root_display: &'a str,
    pub(super) query: &'a str,
    pub(super) output: context_search::ContextPackOutput,
    pub(super) semantic_disabled_reason: Option<String>,
    pub(super) language: context_graph::GraphLanguage,
    pub(super) available_models: &'a [String],
}

pub(super) async fn finalize_context_pack(
    args: FinalizeContextPackArgs<'_>,
) -> Result<rmcp::model::CallToolResult, super::super::super::McpError> {
    let FinalizeContextPackArgs {
        service,
        inputs,
        root_display,
        query,
        mut output,
        semantic_disabled_reason,
        language,
        available_models,
    } = args;

    match inputs.response_mode {
        ResponseMode::Minimal => {
            output.meta.index_state = None;
        }
        ResponseMode::Facts => {}
        ResponseMode::Full => {
            if output.items.is_empty() && semantic_disabled_reason.is_some() {
                let budgets = super::super::super::mcp_default_budgets();
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
                    reason: "Semantic search is disabled; fall back to rg on the most relevant query token.".to_string(),
                });
            }

            if output.items.is_empty() {
                let pattern = fallback::choose_fallback_token(&inputs.query_tokens)
                    .or_else(|| inputs.query_tokens.iter().max_by_key(|t| t.len()).cloned())
                    .unwrap_or_else(|| query.trim().to_string());

                output.next_actions.push(ToolNextAction {
                    tool: "text_search".to_string(),
                    args: serde_json::json!({
                        "path": root_display,
                        "pattern": pattern,
                        "max_results": 80,
                        "case_sensitive": false,
                        "whole_word": true,
                        "response_mode": "facts"
                    }),
                    reason: "No semantic hits; verify the strongest query anchor via text_search (detects typos, wrong root, or stale index).".to_string(),
                });

                output.next_actions.push(ToolNextAction {
                    tool: "repo_onboarding_pack".to_string(),
                    args: serde_json::json!({
                        "path": root_display,
                        "max_chars": 12000,
                        "response_mode": "facts"
                    }),
                    reason: "No semantic hits; re-onboard the repo to confirm the effective root and key docs.".to_string(),
                });
            } else if let Some(token) = fallback::choose_fallback_token(&inputs.query_tokens) {
                if !fallback::items_mention_token(&output.items, &token) {
                    output.next_actions.push(ToolNextAction {
                        tool: "text_search".to_string(),
                        args: serde_json::json!({
                            "path": root_display,
                            "pattern": token,
                            "max_results": 80,
                            "case_sensitive": false,
                            "whole_word": true,
                            "response_mode": "facts"
                        }),
                        reason: "Semantic hits do not mention the key query token; verify the exact term via text_search (often reveals wrong root or stale index).".to_string(),
                    });
                }
            }

            let retry_action = render::build_retry_action(root_display, query, inputs, &output);
            if output.budget.truncated {
                output.next_actions.push(retry_action.clone());
            }
            if let Err(result) = budget::enforce_context_pack_budget(&mut output) {
                return Ok(result);
            }
            if output.budget.truncated && output.next_actions.is_empty() {
                output.next_actions.push(retry_action);
                if let Err(result) = budget::enforce_context_pack_budget(&mut output) {
                    return Ok(result);
                }
            }

            let mut contents =
                render::render_full(inputs, &output, semantic_disabled_reason.as_deref());
            if inputs.flags.trace() {
                trace::append_trace_debug(
                    &mut contents,
                    service,
                    inputs,
                    language,
                    available_models,
                );
            }
            return Ok(render::finish_result(contents, output));
        }
    }

    if let Err(result) = budget::enforce_context_pack_budget(&mut output) {
        return Ok(result);
    }
    let contents = render::render_default(inputs, &output, semantic_disabled_reason.as_deref());
    Ok(render::finish_result(contents, output))
}
