use super::super::{
    call_error, ContextFinderService, ReadPackContext, ReadPackRequest, ResponseMode, ToolResult,
};
use crate::tools::dispatch::router::context_pack::context_pack;
use crate::tools::schemas::context_pack::ContextPackRequest;
use serde_json::Value;

pub(super) async fn fetch_context_pack_json(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    query: &str,
) -> ToolResult<Value> {
    let tool_result = context_pack(
        service,
        ContextPackRequest {
            path: Some(ctx.root_display.clone()),
            query: query.to_string(),
            language: None,
            strategy: None,
            limit: None,
            max_chars: Some(ctx.inner_max_chars),
            include_paths: request.include_paths.clone(),
            exclude_paths: request.exclude_paths.clone(),
            file_pattern: request.file_pattern.clone(),
            max_related_per_primary: None,
            include_docs: request.include_docs,
            prefer_code: request.prefer_code,
            related_mode: None,
            response_mode: request.response_mode,
            trace: Some(false),
            auto_index: None,
            auto_index_budget_ms: None,
        },
    )
    .await
    .map_err(|err| call_error("internal", format!("Error: {err}")))?;

    if tool_result.is_error == Some(true) {
        return Err(tool_result);
    }

    let mut value: Value = tool_result.structured_content.clone().ok_or_else(|| {
        call_error(
            "internal",
            "Error: context_pack returned no structured_content",
        )
    })?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("meta");
        if response_mode != ResponseMode::Full {
            obj.remove("next_actions");
        }
    }

    Ok(value)
}
