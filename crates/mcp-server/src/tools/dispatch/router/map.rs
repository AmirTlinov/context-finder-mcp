use super::super::{
    compute_map_result, decode_map_cursor, CallToolResult, Content, ContextFinderService,
    MapRequest, McpError, ResponseMode, ToolMeta, CURSOR_VERSION,
};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::cursor::cursor_fingerprint;
use crate::tools::schemas::ToolNextAction;
use context_indexer::root_fingerprint;
use serde_json::json;

use super::cursor_alias::{compact_cursor_alias, expand_cursor_alias};
use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_cursor_with_meta,
    invalid_cursor_with_meta_details, invalid_request_with_meta, meta_for_request,
};

/// Get project structure overview
pub(in crate::tools::dispatch) async fn map(
    service: &ContextFinderService,
    mut request: MapRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Minimal);
    let mut cursor_ignored_note: Option<&'static str> = None;

    if let Some(cursor) = request.cursor.as_deref() {
        match expand_cursor_alias(service, cursor).await {
            Ok(expanded) => request.cursor = Some(expanded),
            Err(message) => {
                let meta = if response_mode == ResponseMode::Full {
                    meta_for_request(service, request.path.as_deref()).await
                } else {
                    ToolMeta::default()
                };
                return Ok(invalid_cursor_with_meta(message, meta));
            }
        }
    }

    let cursor_payload = match request
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(cursor) => match decode_map_cursor(cursor) {
            Ok(decoded) => Some(decoded),
            Err(err) => {
                let meta = if response_mode == ResponseMode::Full {
                    meta_for_request(service, request.path.as_deref()).await
                } else {
                    ToolMeta::default()
                };
                return Ok(invalid_cursor_with_meta(
                    format!("Invalid cursor: {err}"),
                    meta,
                ));
            }
        },
        None => None,
    };

    let path_missing = match request.path.as_deref().map(str::trim) {
        Some(value) => value.is_empty(),
        None => true,
    };
    if path_missing {
        if let Some(decoded) = cursor_payload.as_ref() {
            if decoded.v == CURSOR_VERSION && (decoded.tool == "tree" || decoded.tool == "map") {
                if let Some(root) = decoded.root.as_deref().map(str::trim) {
                    if !root.is_empty() {
                        let session_root_display =
                            { service.session.lock().await.root_display.clone() };
                        if let Some(session_root_display) = session_root_display {
                            if session_root_display != root {
                                return Ok(invalid_cursor_with_meta(
                                    "Invalid cursor: cursor refers to a different project root than the current session; pass `path` to switch projects.",
                                    ToolMeta {
                                        root_fingerprint: Some(root_fingerprint(
                                            &session_root_display,
                                        )),
                                        ..ToolMeta::default()
                                    },
                                ));
                            }
                        }
                        request.path = Some(root.to_string());
                    }
                }
            }
        }
    }

    let (root, root_display) = match service
        .resolve_root_no_daemon_touch(request.path.as_deref())
        .await
    {
        Ok(value) => value,
        Err(message) => {
            let meta = if response_mode == ResponseMode::Full {
                meta_for_request(service, request.path.as_deref()).await
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
    let meta_for_structured = if response_mode == ResponseMode::Full {
        meta_for_output.clone()
    } else {
        provenance_meta.clone()
    };

    let depth = request
        .depth
        .or_else(|| cursor_payload.as_ref().map(|c| c.depth))
        .unwrap_or(2)
        .clamp(1, 4);
    let limit = request
        .limit
        .or_else(|| {
            cursor_payload
                .as_ref()
                .and_then(|c| (c.limit > 0).then_some(c.limit))
        })
        .unwrap_or(10);

    let offset = if let Some(decoded) = cursor_payload.as_ref() {
        if decoded.v != CURSOR_VERSION || (decoded.tool != "tree" && decoded.tool != "map") {
            return Ok(invalid_cursor_with_meta(
                "Invalid cursor: wrong tool (expected tree)",
                meta_for_output.clone(),
            ));
        }
        if let Some(hash) = decoded.root_hash {
            if hash != cursor_fingerprint(&root_display) {
                let expected_root_fingerprint = meta_for_output
                    .root_fingerprint
                    .unwrap_or_else(|| root_fingerprint(&root_display));
                return Ok(invalid_cursor_with_meta_details(
                    "Invalid cursor: different root",
                    meta_for_output.clone(),
                    json!({
                        "expected_root_fingerprint": expected_root_fingerprint,
                        "cursor_root_fingerprint": Some(hash),
                    }),
                ));
            }
        } else if decoded.root.as_deref() != Some(&root_display) {
            let expected_root_fingerprint = meta_for_output
                .root_fingerprint
                .unwrap_or_else(|| root_fingerprint(&root_display));
            let cursor_root_fingerprint = decoded.root.as_deref().map(root_fingerprint);
            return Ok(invalid_cursor_with_meta_details(
                "Invalid cursor: different root",
                meta_for_output.clone(),
                json!({
                    "expected_root_fingerprint": expected_root_fingerprint,
                    "cursor_root_fingerprint": cursor_root_fingerprint,
                }),
            ));
        }
        if decoded.depth != depth {
            // Depth changes the aggregation shape. Treat this as a recoverable mismatch: ignore
            // the cursor and restart from offset 0 (instead of failing the tool call).
            cursor_ignored_note = Some("cursor ignored: different depth (restarting pagination)");
            0usize
        } else {
            decoded.offset
        }
    } else {
        0usize
    };

    let mut result = match compute_map_result(&root, &root_display, depth, limit, offset).await {
        Ok(result) => result,
        Err(err) => {
            return Ok(internal_error_with_meta(
                format!("Error: {err:#}"),
                meta_for_output.clone(),
            ))
        }
    };
    match response_mode {
        ResponseMode::Full => {
            result.meta = Some(meta_for_output);
        }
        ResponseMode::Facts => {
            result.meta = Some(provenance_meta.clone());
            // Facts-mode keeps high-signal structure, but avoids diagnostics that can dominate
            // the payload in tight-loop agent usage.
            result.total_chunks = None;
            result.total_lines = None;
            for dir in &mut result.directories {
                dir.coverage_pct = None;
                dir.top_symbols = None;
            }
        }
        ResponseMode::Minimal => {
            result.meta = Some(provenance_meta.clone());
            result.total_files = None;
            result.total_chunks = None;
            result.total_lines = None;
            for dir in &mut result.directories {
                dir.files = None;
                dir.chunks = None;
                dir.coverage_pct = None;
                dir.top_symbols = None;
            }
        }
    }
    if let Some(cursor) = result.next_cursor.take() {
        result.next_cursor = Some(compact_cursor_alias(service, cursor).await);
    }
    if response_mode == ResponseMode::Full {
        if let Some(cursor) = result.next_cursor.clone() {
            result.next_actions = Some(vec![ToolNextAction {
                tool: "tree".to_string(),
                args: json!({
                    "path": root_display,
                    "depth": depth,
                    "limit": limit,
                    "cursor": cursor,
                }),
                reason: "Continue tree pagination with the next cursor.".to_string(),
            }]);
        }
    }

    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!("tree: {} directories", result.directories.len()));
    doc.push_root_fingerprint(meta_for_structured.root_fingerprint);
    if let Some(note) = cursor_ignored_note {
        doc.push_note(note);
    }
    for dir in &result.directories {
        match (dir.files, dir.chunks) {
            (Some(files), Some(chunks)) => {
                doc.push_line(&format!("{} (files={files}, chunks={chunks})", dir.path));
            }
            _ => {
                doc.push_line(&dir.path);
            }
        }
    }
    if result.truncated {
        if let Some(cursor) = result.next_cursor.as_deref() {
            doc.push_cursor(cursor);
        }
    }
    let output = CallToolResult::success(vec![Content::text(doc.finish())]);
    Ok(attach_structured_content(
        output,
        &result,
        meta_for_structured,
        "map",
    ))
}
