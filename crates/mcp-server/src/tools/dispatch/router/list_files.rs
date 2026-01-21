use super::super::{
    compute_list_files_result, decode_list_files_cursor, finalize_list_files_budget,
    CallToolResult, Content, ContextFinderService, ListFilesRequest, McpError, ResponseMode,
    ToolMeta, CURSOR_VERSION,
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

/// List project files within the project root (safe file enumeration for agents).
pub(in crate::tools::dispatch) async fn list_files(
    service: &ContextFinderService,
    mut request: ListFilesRequest,
) -> Result<CallToolResult, McpError> {
    const DEFAULT_LIMIT: usize = 200;
    const MAX_LIMIT: usize = 50_000;
    const DEFAULT_MAX_CHARS: usize = 2_000;
    const MAX_MAX_CHARS: usize = 500_000;

    let response_mode = request.response_mode.unwrap_or(ResponseMode::Minimal);
    let mut cursor_ignored_note: Option<&'static str> = None;

    let requested_allow_secrets = request.allow_secrets;
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

    let path_missing = match request.path.as_deref().map(str::trim) {
        Some(value) => value.is_empty(),
        None => true,
    };
    if path_missing {
        if let Some(cursor) = request
            .cursor
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Ok(decoded) = decode_list_files_cursor(cursor) {
                if decoded.v == CURSOR_VERSION
                    && (decoded.tool == "ls" || decoded.tool == "list_files")
                {
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
    }

    let mut hints: Vec<String> = Vec::new();
    if let Some(pattern) = request.file_pattern.as_deref() {
        if !pattern.trim().is_empty() {
            hints.push(pattern.to_string());
        }
    }
    let (root, root_display) = match service
        .resolve_root_with_hints_no_daemon_touch(request.path.as_deref(), &hints)
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

    let normalized_file_pattern = request
        .file_pattern
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let (cursor_last_file, cursor_allow_secrets, cursor_limit, cursor_max_chars) =
        if let Some(cursor) = request
            .cursor
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let decoded = match decode_list_files_cursor(cursor) {
                Ok(v) => v,
                Err(err) => {
                    return Ok(invalid_cursor_with_meta(
                        format!("Invalid cursor: {err}"),
                        meta_for_output.clone(),
                    ));
                }
            };
            if decoded.v != CURSOR_VERSION || (decoded.tool != "ls" && decoded.tool != "list_files")
            {
                return Ok(invalid_cursor_with_meta(
                    "Invalid cursor: wrong tool (expected ls)",
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
            if decoded.file_pattern != normalized_file_pattern {
                // Pattern changes the file universe. Restart from the beginning.
                cursor_ignored_note =
                    Some("cursor ignored: different file_pattern (restarting pagination)");
                (None, None, None, None)
            } else if let Some(allow_secrets) = requested_allow_secrets {
                if decoded.allow_secrets != allow_secrets {
                    // The caller is explicitly changing allow_secrets; restart from the beginning.
                    cursor_ignored_note =
                        Some("cursor ignored: different allow_secrets (restarting pagination)");
                    (None, None, None, None)
                } else {
                    (
                        Some(decoded.last_file),
                        Some(decoded.allow_secrets),
                        Some(decoded.limit),
                        Some(decoded.max_chars),
                    )
                }
            } else {
                (
                    Some(decoded.last_file),
                    Some(decoded.allow_secrets),
                    Some(decoded.limit),
                    Some(decoded.max_chars),
                )
            }
        } else {
            (None, None, None, None)
        };

    let limit = request
        .limit
        .or(cursor_limit.filter(|n| *n > 0))
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let max_chars = request
        .max_chars
        .or(cursor_max_chars.filter(|n| *n > 0))
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(1, MAX_MAX_CHARS);

    let allow_secrets = requested_allow_secrets
        .or(cursor_allow_secrets)
        .unwrap_or(false);
    let mut result = match compute_list_files_result(
        &root,
        &root_display,
        request.file_pattern.as_deref(),
        limit,
        max_chars,
        allow_secrets,
        cursor_last_file.as_deref(),
    )
    .await
    {
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
            result.meta = Some(meta_for_output.clone());
        }
        ResponseMode::Facts => {
            result.meta = Some(provenance_meta.clone());
        }
        ResponseMode::Minimal => {
            result.meta = Some(provenance_meta.clone());
            result.source = None;
            result.file_pattern = None;
            result.scanned_files = None;
            result.returned = None;
            result.used_chars = None;
            result.limit = None;
            result.max_chars = None;
        }
    }
    if let Err(_err) = finalize_list_files_budget(&mut result, max_chars) {
        // Fail-soft: never error solely due to budget envelope constraints.
        //
        // At this point the structured payload is too large for the requested budget. Keep the
        // `.context` output usable by returning a minimal payload (and preserving any cursor when
        // present so callers can retry with a larger budget).
        result.truncated = true;
        result.truncation = Some(context_protocol::BudgetTruncation::MaxChars);
        result.files.clear();
    }
    if let Some(cursor) = result.next_cursor.take() {
        result.next_cursor = Some(compact_cursor_alias(service, cursor).await);
    }
    if response_mode == ResponseMode::Full {
        if let Some(cursor) = result.next_cursor.clone() {
            result.next_actions = Some(vec![ToolNextAction {
                tool: "ls".to_string(),
                args: json!({
                    "path": root_display,
                    "file_pattern": normalized_file_pattern,
                    "limit": limit,
                    "max_chars": max_chars,
                    "allow_secrets": allow_secrets,
                    "cursor": cursor,
                }),
                reason: "Continue ls pagination with the next cursor.".to_string(),
            }]);
        }
    }

    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!("{} files", result.files.len()));
    doc.push_root_fingerprint(meta_for_output.root_fingerprint);
    if let Some(note) = cursor_ignored_note {
        doc.push_note(note);
    }
    for file in &result.files {
        doc.push_line(file);
    }
    if result.truncated {
        if let Some(cursor) = result.next_cursor.as_deref() {
            doc.push_cursor(cursor);
        }
    }
    let (rendered, _truncated) = doc.finish_bounded(max_chars);
    let output = CallToolResult::success(vec![Content::text(rendered)]);
    Ok(attach_structured_content(
        output,
        &result,
        meta_for_output,
        "list_files",
    ))
}
