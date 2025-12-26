use super::super::{
    compute_list_files_result, decode_list_files_cursor, CallToolResult, Content,
    ContextFinderService, ListFilesRequest, McpError, CURSOR_VERSION,
};

/// List project files within the project root (safe file enumeration for agents).
pub(in crate::tools::dispatch) async fn list_files(
    service: &ContextFinderService,
    request: ListFilesRequest,
) -> Result<CallToolResult, McpError> {
    const DEFAULT_LIMIT: usize = 200;
    const MAX_LIMIT: usize = 50_000;
    const DEFAULT_MAX_CHARS: usize = 20_000;
    const MAX_MAX_CHARS: usize = 500_000;

    let (root, root_display) = match service.resolve_root(request.path.as_deref()).await {
        Ok(value) => value,
        Err(message) => return Ok(CallToolResult::error(vec![Content::text(message)])),
    };

    let limit = request.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let max_chars = request
        .max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(1, MAX_MAX_CHARS);

    let normalized_file_pattern = request
        .file_pattern
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let cursor_last_file = if let Some(cursor) = request
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let decoded = match decode_list_files_cursor(cursor) {
            Ok(v) => v,
            Err(err) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid cursor: {err}"
                ))]));
            }
        };
        if decoded.v != CURSOR_VERSION || decoded.tool != "list_files" {
            return Ok(CallToolResult::error(vec![Content::text(
                "Invalid cursor: wrong tool",
            )]));
        }
        if decoded.root != root_display {
            return Ok(CallToolResult::error(vec![Content::text(
                "Invalid cursor: different root",
            )]));
        }
        if decoded.file_pattern != normalized_file_pattern {
            return Ok(CallToolResult::error(vec![Content::text(
                "Invalid cursor: different file_pattern",
            )]));
        }
        Some(decoded.last_file)
    } else {
        None
    };
    let mut result = match compute_list_files_result(
        &root,
        &root_display,
        request.file_pattern.as_deref(),
        limit,
        max_chars,
        cursor_last_file.as_deref(),
    )
    .await
    {
        Ok(result) => result,
        Err(err) => {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: {err:#}"
            ))]));
        }
    };
    result.meta = Some(service.tool_meta(&root).await);

    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&result).unwrap_or_default(),
    )]))
}
