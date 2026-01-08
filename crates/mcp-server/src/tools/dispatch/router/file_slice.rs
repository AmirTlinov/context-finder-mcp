use super::super::{
    compute_file_slice_result, decode_cursor, CallToolResult, Content, ContextFinderService,
    FileSliceCursorV1, FileSliceRequest, McpError, ResponseMode, ToolMeta, CURSOR_VERSION,
};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::schemas::ToolNextAction;
use context_indexer::root_fingerprint;
use serde_json::json;

use super::cursor_alias::{compact_cursor_alias, expand_cursor_alias};
use super::error::{
    attach_structured_content, invalid_cursor_with_meta, invalid_request_with_meta,
    meta_for_request,
};

/// Read a bounded slice of a file within the project root (safe file access for agents).
pub(in crate::tools::dispatch) async fn file_slice(
    service: &ContextFinderService,
    request: &FileSliceRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Minimal);

    let expanded_cursor = if let Some(cursor) = request.cursor.as_deref() {
        match expand_cursor_alias(service, cursor).await {
            Ok(cursor) => Some(cursor),
            Err(message) => {
                let meta = if response_mode == ResponseMode::Full {
                    meta_for_request(service, request.path.as_deref()).await
                } else {
                    ToolMeta::default()
                };
                return Ok(invalid_cursor_with_meta(message, meta));
            }
        }
    } else {
        None
    };

    let cursor_payload = expanded_cursor.as_deref().and_then(|cursor| {
        decode_cursor::<FileSliceCursorV1>(cursor)
            .ok()
            .filter(|decoded| decoded.v == CURSOR_VERSION && decoded.tool == "file_slice")
    });

    const DEFAULT_MAX_CHARS: usize = 2_000;
    const MAX_MAX_CHARS: usize = 500_000;
    let requested_max_chars = request
        .max_chars
        .or_else(|| cursor_payload.as_ref().map(|c| c.max_chars))
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(1, MAX_MAX_CHARS);

    let mut path = request.path.clone();
    let path_missing = match path.as_deref().map(str::trim) {
        Some(value) => value.is_empty(),
        None => true,
    };
    if path_missing {
        if let Some(decoded) = cursor_payload.as_ref() {
            if let Some(root) = decoded.root.as_deref().map(str::trim) {
                if !root.is_empty() {
                    path = Some(root.to_string());
                }
            }
        }
    }

    let mut hints: Vec<String> = Vec::new();
    if let Some(file) = request.file.as_deref() {
        hints.push(file.to_string());
    }
    let (root, root_display) = match service
        .resolve_root_with_hints_no_daemon_touch(path.as_deref(), &hints)
        .await
    {
        Ok(value) => value,
        Err(message) => {
            let meta = if response_mode == ResponseMode::Full {
                meta_for_request(service, path.as_deref()).await
            } else {
                ToolMeta::default()
            };
            return Ok(invalid_request_with_meta(message, meta, None, Vec::new()));
        }
    };
    let provenance_meta = ToolMeta {
        root_fingerprint: Some(root_fingerprint(&root_display)),
        ..ToolMeta::default()
    };
    let meta_for_output = if response_mode == ResponseMode::Full {
        service.tool_meta(&root).await
    } else {
        provenance_meta.clone()
    };

    let compute_request = FileSliceRequest {
        path: path.clone(),
        file: request.file.clone(),
        start_line: request.start_line,
        max_lines: request.max_lines,
        max_chars: request.max_chars,
        format: request.format,
        response_mode: Some(response_mode),
        allow_secrets: request.allow_secrets,
        cursor: expanded_cursor,
    };
    let mut result = match compute_file_slice_result(&root, &root_display, &compute_request) {
        Ok(result) => result,
        Err(msg) => {
            if msg.trim_start().starts_with("Invalid cursor") {
                return Ok(invalid_cursor_with_meta(msg, meta_for_output.clone()));
            }
            return Ok(invalid_request_with_meta(
                msg,
                meta_for_output.clone(),
                None,
                Vec::new(),
            ));
        }
    };
    match response_mode {
        ResponseMode::Full => {
            result.meta = Some(meta_for_output.clone());
        }
        ResponseMode::Facts => {
            result.meta = Some(provenance_meta.clone());
            result.file_size_bytes = None;
            result.file_mtime_ms = None;
            result.content_sha256 = None;
        }
        ResponseMode::Minimal => {
            result.meta = Some(provenance_meta.clone());
            result.returned_lines = None;
            result.used_chars = None;
            result.max_lines = None;
            result.max_chars = None;
            result.file_size_bytes = None;
            result.file_mtime_ms = None;
            result.content_sha256 = None;
        }
    }

    if let Some(cursor) = result.next_cursor.take() {
        result.next_cursor = Some(compact_cursor_alias(service, cursor).await);
    }

    if response_mode == ResponseMode::Full {
        if let Some(cursor) = result.next_cursor.clone() {
            result.next_actions = Some(vec![ToolNextAction {
                tool: "file_slice".to_string(),
                args: json!({
                    "path": root_display,
                    "cursor": cursor,
                }),
                reason: "Continue file_slice pagination with the next cursor.".to_string(),
            }]);
        }
    }

    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!(
        "{} (lines {}–{})",
        result.file, result.start_line, result.end_line
    ));
    doc.push_root_fingerprint(meta_for_output.root_fingerprint);
    doc.push_ref_header(&result.file, result.start_line, Some("file slice"));
    doc.push_block_smart(&result.content);
    if result.truncated {
        if let Some(cursor) = result.next_cursor.as_deref() {
            doc.push_cursor(cursor);
        }
    }
    let (rendered, envelope_truncated) = doc.finish_bounded(requested_max_chars);
    if envelope_truncated {
        // Fail-soft: keep the tool usable under tight budgets (never error solely due to envelope
        // overhead). The structured payload can still indicate the original truncation/cursor
        // state; the `.context` output is what must stay within `max_chars`.
        result.truncated = true;
    }

    let output = CallToolResult::success(vec![Content::text(rendered)]);
    Ok(attach_structured_content(
        output,
        &result,
        meta_for_output,
        "file_slice",
    ))
}
