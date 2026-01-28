use super::super::super::{ContextFinderService, ResponseMode};
use super::inputs::ContextPackInputs;
use super::{budget, render, trace};

#[path = "next_actions.rs"]
mod next_actions;

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
            // v2 explicitly opts into a small trust envelope even under "minimal".
            if inputs.format_version != 2 {
                output.meta.index_state = None;
                output.meta.trust = None;
            }
        }
        ResponseMode::Facts => {}
        ResponseMode::Full => {
            next_actions::add_full_mode_next_actions(
                inputs,
                root_display,
                query,
                &mut output,
                semantic_disabled_reason.as_deref(),
            );

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

            next_actions::maybe_cap_next_actions_for_v2(inputs, &mut output);
            if let Err(result) = budget::enforce_context_pack_budget(&mut output) {
                return Ok(result);
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

    next_actions::maybe_add_low_noise_next_actions(
        inputs,
        root_display,
        query,
        &mut output,
        semantic_disabled_reason.as_deref(),
    );

    if let Err(result) = budget::enforce_context_pack_budget(&mut output) {
        return Ok(result);
    }
    let contents = render::render_default(inputs, &output, semantic_disabled_reason.as_deref());
    Ok(render::finish_result(contents, output))
}
