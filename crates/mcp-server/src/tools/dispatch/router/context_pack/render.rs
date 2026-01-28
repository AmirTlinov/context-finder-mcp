#[path = "render_default.rs"]
mod render_default;
#[path = "render_full.rs"]
mod render_full;
#[path = "render_meta.rs"]
pub(super) mod render_meta;

use super::super::super::{CallToolResult, Content};
use super::super::error::internal_error_with_meta;
use super::inputs::ContextPackInputs;
use crate::tools::schemas::ToolNextAction;
use context_search::ContextPackOutput;

pub(super) use render_meta::{
    maybe_push_trust_micro_meta, push_next_actions, push_next_actions_v2, push_v2_envelope,
};

pub(super) fn build_retry_action(
    root_display: &str,
    query: &str,
    inputs: &ContextPackInputs,
    output: &ContextPackOutput,
) -> ToolNextAction {
    let next_max_chars = output.budget.max_chars.saturating_mul(2).min(500_000);
    let mut args = serde_json::json!({
        "path": root_display,
        "query": query,
        "max_chars": next_max_chars,
    });
    if let Some(obj) = args.as_object_mut() {
        if !inputs.include_paths.is_empty() {
            obj.insert(
                "include_paths".to_string(),
                serde_json::to_value(&inputs.include_paths).unwrap_or_default(),
            );
        }
        if !inputs.exclude_paths.is_empty() {
            obj.insert(
                "exclude_paths".to_string(),
                serde_json::to_value(&inputs.exclude_paths).unwrap_or_default(),
            );
        }
        if let Some(pattern) = inputs.file_pattern.as_deref() {
            obj.insert(
                "file_pattern".to_string(),
                serde_json::Value::String(pattern.to_string()),
            );
        }
    }

    ToolNextAction {
        tool: "context_pack".to_string(),
        args,
        reason: "Retry context_pack with a larger max_chars budget.".to_string(),
    }
}

pub(super) fn render_full(
    inputs: &ContextPackInputs,
    output: &ContextPackOutput,
    semantic_disabled_reason: Option<&str>,
) -> Vec<Content> {
    render_full::render_full(inputs, output, semantic_disabled_reason)
}

pub(super) fn render_default(
    inputs: &ContextPackInputs,
    output: &ContextPackOutput,
    semantic_disabled_reason: Option<&str>,
) -> Vec<Content> {
    render_default::render_default(inputs, output, semantic_disabled_reason)
}

pub(super) fn finish_result(contents: Vec<Content>, output: ContextPackOutput) -> CallToolResult {
    let mut result = CallToolResult::success(contents);
    match serde_json::to_value(&output) {
        Ok(structured) => {
            result.structured_content = Some(structured);
        }
        Err(err) => {
            return internal_error_with_meta(
                format!("Error: failed to serialize context_pack output ({err})"),
                output.meta.clone(),
            );
        }
    }
    result
}
