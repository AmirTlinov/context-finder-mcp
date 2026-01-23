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
    attach_structured_content, cursor_mismatch_with_meta_details, internal_error_with_meta,
    invalid_cursor_with_meta, invalid_cursor_with_meta_details, invalid_request_with_meta,
    meta_for_request,
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
                    && (decoded.tool == "find"
                        || decoded.tool == "ls"
                        || decoded.tool == "list_files")
                {
                    if let Some(root) = decoded.root.as_deref().map(str::trim) {
                        if !root.is_empty() {
                            let session_root_display =
                                { service.session.lock().await.root_display() };
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

    let mut cursor_file_pattern: Option<String> = None;
    let (cursor_last_file, cursor_allow_secrets, cursor_limit, cursor_max_chars) = if let Some(
        cursor,
    ) = request
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let cursor = cursor.to_string();
        let decoded = match decode_list_files_cursor(&cursor) {
            Ok(v) => v,
            Err(err) => {
                return Ok(invalid_cursor_with_meta(
                    format!("Invalid cursor: {err}"),
                    meta_for_output.clone(),
                ));
            }
        };
        if decoded.v != CURSOR_VERSION
            || (decoded.tool != "find" && decoded.tool != "ls" && decoded.tool != "list_files")
        {
            return Ok(invalid_cursor_with_meta(
                "Invalid cursor: wrong tool (expected find)",
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

        if normalized_file_pattern.is_some() && decoded.file_pattern != normalized_file_pattern {
            let cursor_for_actions = compact_cursor_alias(service, cursor.clone()).await;
            let effective_limit = request
                .limit
                .or(Some(decoded.limit).filter(|n| *n > 0))
                .unwrap_or(DEFAULT_LIMIT)
                .clamp(1, MAX_LIMIT);
            let effective_max_chars = request
                .max_chars
                .or(Some(decoded.max_chars).filter(|n| *n > 0))
                .unwrap_or(DEFAULT_MAX_CHARS)
                .clamp(1, MAX_MAX_CHARS);
            let restart_allow_secrets = requested_allow_secrets.unwrap_or(decoded.allow_secrets);
            let mut next_actions: Vec<ToolNextAction> = Vec::new();
            next_actions.push(ToolNextAction {
                tool: "find".to_string(),
                args: json!({ "path": root_display, "cursor": cursor_for_actions }),
                reason:
                    "Continue find pagination using the cursor only (drop file_pattern override)."
                        .to_string(),
            });
            next_actions.push(ToolNextAction {
                tool: "find".to_string(),
                args: json!({
                    "path": root_display,
                    "file_pattern": normalized_file_pattern,
                    "limit": effective_limit,
                    "max_chars": effective_max_chars,
                    "allow_secrets": restart_allow_secrets,
                }),
                reason: "Restart find without cursor using the new file_pattern.".to_string(),
            });
            return Ok(cursor_mismatch_with_meta_details(
                "Cursor mismatch: request file_pattern differs from cursor",
                meta_for_output.clone(),
                json!({
                    "mismatch": "file_pattern",
                    "cursor_file_pattern": decoded.file_pattern,
                    "request_file_pattern": normalized_file_pattern,
                }),
                Some(
                    "Repeat the call with cursor only, or drop cursor to restart with new options."
                        .to_string(),
                ),
                next_actions,
            ));
        }
        if let Some(allow_secrets) = requested_allow_secrets {
            if decoded.allow_secrets != allow_secrets {
                let cursor_for_actions = compact_cursor_alias(service, cursor.clone()).await;
                let effective_limit = request
                    .limit
                    .or(Some(decoded.limit).filter(|n| *n > 0))
                    .unwrap_or(DEFAULT_LIMIT)
                    .clamp(1, MAX_LIMIT);
                let effective_max_chars = request
                    .max_chars
                    .or(Some(decoded.max_chars).filter(|n| *n > 0))
                    .unwrap_or(DEFAULT_MAX_CHARS)
                    .clamp(1, MAX_MAX_CHARS);
                let restart_file_pattern = normalized_file_pattern
                    .clone()
                    .or(decoded.file_pattern.clone());
                let mut next_actions: Vec<ToolNextAction> = Vec::new();
                next_actions.push(ToolNextAction {
                    tool: "find".to_string(),
                    args: json!({ "path": root_display, "cursor": cursor_for_actions }),
                    reason:
                        "Continue find pagination using the cursor only (drop allow_secrets override)."
                            .to_string(),
                });
                next_actions.push(ToolNextAction {
                    tool: "find".to_string(),
                    args: json!({
                        "path": root_display,
                        "file_pattern": restart_file_pattern,
                        "limit": effective_limit,
                        "max_chars": effective_max_chars,
                        "allow_secrets": allow_secrets,
                    }),
                    reason: "Restart find without cursor using the new allow_secrets.".to_string(),
                });
                return Ok(cursor_mismatch_with_meta_details(
                        "Cursor mismatch: request allow_secrets differs from cursor",
                        meta_for_output.clone(),
                        json!({
                            "mismatch": "allow_secrets",
                            "cursor_allow_secrets": decoded.allow_secrets,
                            "request_allow_secrets": allow_secrets,
                        }),
                        Some("Repeat the call with cursor only, or drop cursor to restart with new options.".to_string()),
                        next_actions,
                    ));
            }
        }

        cursor_file_pattern = decoded.file_pattern.clone();
        (
            Some(decoded.last_file),
            Some(decoded.allow_secrets),
            Some(decoded.limit),
            Some(decoded.max_chars),
        )
    } else {
        (None, None, None, None)
    };

    let effective_file_pattern = normalized_file_pattern.clone().or(cursor_file_pattern);
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
        effective_file_pattern.as_deref(),
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
                tool: "find".to_string(),
                args: json!({
                    "path": root_display,
                    "file_pattern": effective_file_pattern,
                    "limit": limit,
                    "max_chars": max_chars,
                    "allow_secrets": allow_secrets,
                    "cursor": cursor,
                }),
                reason: "Continue find pagination with the next cursor.".to_string(),
            }]);
        }
        if result.next_actions.is_none() && result.files.is_empty() {
            let mut next_actions: Vec<ToolNextAction> = Vec::new();
            next_actions.push(ToolNextAction {
                tool: "tree".to_string(),
                args: json!({
                    "path": root_display,
                    "depth": 2,
                    "limit": 50,
                }),
                reason: "Show a directory overview (find lists file paths only).".to_string(),
            });
            if effective_file_pattern.is_some() {
                next_actions.push(ToolNextAction {
                    tool: "find".to_string(),
                    args: json!({
                        "path": root_display,
                        "limit": limit,
                        "max_chars": max_chars,
                        "allow_secrets": allow_secrets,
                    }),
                    reason: "Retry find without file_pattern filtering.".to_string(),
                });
            }
            result.next_actions = Some(next_actions);
        }
    }

    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!("{} files", result.files.len()));
    if response_mode != ResponseMode::Minimal {
        doc.push_root_fingerprint(meta_for_output.root_fingerprint);
    }
    if result.files.is_empty() {
        if result.truncated {
            doc.push_note("hint: output is truncated; retry with a larger max_chars");
        } else {
            doc.push_note("hint: no matching file paths");
            doc.push_note("hint: find lists file paths only; for directories use ls/tree");
            doc.push_note("next: ls or tree (directory overview)");
        }
        if !result.truncated && !allow_secrets {
            doc.push_note(
                "hint: secret paths (.env, keys, *.pem) are hidden by default; pass allow_secrets=true if you explicitly need them",
            );
        }
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
        "find",
    ))
}
