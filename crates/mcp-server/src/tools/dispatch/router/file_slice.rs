use super::super::{
    compute_file_slice_result, CallToolResult, Content, ContextFinderService, FileSliceRequest,
    McpError,
};

/// Read a bounded slice of a file within the project root (safe file access for agents).
pub(in crate::tools::dispatch) async fn file_slice(
    service: &ContextFinderService,
    request: &FileSliceRequest,
) -> Result<CallToolResult, McpError> {
    let (root, root_display) = match service.resolve_root(request.path.as_deref()).await {
        Ok(value) => value,
        Err(message) => return Ok(CallToolResult::error(vec![Content::text(message)])),
    };
    let mut result = match compute_file_slice_result(&root, &root_display, request) {
        Ok(result) => result,
        Err(msg) => return Ok(CallToolResult::error(vec![Content::text(msg)])),
    };
    result.meta = Some(service.tool_meta(&root).await);

    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&result).unwrap_or_default(),
    )]))
}
