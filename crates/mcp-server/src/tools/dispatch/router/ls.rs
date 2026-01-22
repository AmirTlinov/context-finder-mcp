use super::super::{
    compute_ls_result, decode_ls_cursor, finalize_ls_budget, CallToolResult, Content,
    ContextFinderService, LsRequest, McpError, ResponseMode, ToolMeta, CURSOR_VERSION,
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

/// List directory entries (ls-like, names-only, bounded).
pub(in crate::tools::dispatch) async fn ls(
    service: &ContextFinderService,
    mut request: LsRequest,
) -> Result<CallToolResult, McpError> {
    const DEFAULT_LIMIT: usize = 200;
    const MAX_LIMIT: usize = 50_000;
    const DEFAULT_MAX_CHARS: usize = 2_000;
    const MAX_MAX_CHARS: usize = 500_000;

    // Product promise: `ls` should behave like shell `ls -a` by default (hidden entries included).
    const DEFAULT_ALL: bool = true;
    const DEFAULT_ALLOW_SECRETS: bool = true;

    let response_mode = request.response_mode.unwrap_or(ResponseMode::Minimal);

    let requested_all = request.all;
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
            if let Ok(decoded) = decode_ls_cursor(cursor) {
                if decoded.v == CURSOR_VERSION && decoded.tool == "ls" {
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
    if let Some(dir) = request.dir.as_deref() {
        if !dir.trim().is_empty() {
            hints.push(dir.to_string());
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

    let cursor_payload = if let Some(cursor) = request.cursor.as_deref().map(str::trim) {
        if cursor.is_empty() {
            None
        } else {
            match decode_ls_cursor(cursor) {
                Ok(decoded) => Some(decoded),
                Err(err) => {
                    return Ok(invalid_cursor_with_meta_details(
                        format!("Invalid cursor: {err}"),
                        meta_for_output.clone(),
                        json!({ "cursor": cursor }),
                    ));
                }
            }
        }
    } else {
        None
    };

    if let Some(decoded) = cursor_payload.as_ref() {
        if decoded.v != CURSOR_VERSION || decoded.tool != "ls" {
            return Ok(invalid_cursor_with_meta(
                "Invalid cursor: wrong tool (expected ls)",
                meta_for_output.clone(),
            ));
        }
        if let Some(hash) = decoded.root_hash {
            if hash != cursor_fingerprint(&root_display) {
                return Ok(invalid_cursor_with_meta(
                    "Invalid cursor: different root",
                    meta_for_output.clone(),
                ));
            }
        } else if decoded.root.as_deref() != Some(&root_display) {
            return Ok(invalid_cursor_with_meta(
                "Invalid cursor: different root",
                meta_for_output.clone(),
            ));
        }

        let normalized_dir = request
            .dir
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(".");
        if request.dir.is_some() && decoded.dir != normalized_dir {
            let cursor_for_actions =
                compact_cursor_alias(service, request.cursor.clone().unwrap_or_default()).await;
            let mut next_actions: Vec<ToolNextAction> = Vec::new();
            next_actions.push(ToolNextAction {
                tool: "ls".to_string(),
                args: json!({ "path": root_display, "cursor": cursor_for_actions }),
                reason: "Continue pagination using the cursor only (drop dir override)."
                    .to_string(),
            });
            next_actions.push(ToolNextAction {
                tool: "ls".to_string(),
                args: json!({ "path": root_display, "dir": normalized_dir }),
                reason: "Restart ls in the requested directory without cursor.".to_string(),
            });
            return Ok(cursor_mismatch_with_meta_details(
                "Cursor mismatch: request dir differs from cursor",
                meta_for_output.clone(),
                json!({
                    "mismatch": "dir",
                    "cursor_dir": decoded.dir,
                    "request_dir": normalized_dir,
                }),
                Some(
                    "Repeat the call with cursor only, or drop cursor to restart with new options."
                        .to_string(),
                ),
                next_actions,
            ));
        }

        if let Some(all) = requested_all {
            if decoded.all != all {
                return Ok(cursor_mismatch_with_meta_details(
                    "Cursor mismatch: request all differs from cursor",
                    meta_for_output.clone(),
                    json!({ "mismatch": "all", "cursor_all": decoded.all, "request_all": all }),
                    Some("Repeat the call with cursor only, or drop cursor to restart with new options.".to_string()),
                    Vec::new(),
                ));
            }
        }
        if let Some(allow_secrets) = requested_allow_secrets {
            if decoded.allow_secrets != allow_secrets {
                return Ok(cursor_mismatch_with_meta_details(
                    "Cursor mismatch: request allow_secrets differs from cursor",
                    meta_for_output.clone(),
                    json!({
                        "mismatch": "allow_secrets",
                        "cursor_allow_secrets": decoded.allow_secrets,
                        "request_allow_secrets": allow_secrets
                    }),
                    Some("Repeat the call with cursor only, or drop cursor to restart with new options.".to_string()),
                    Vec::new(),
                ));
            }
        }
    }

    let effective_dir = request
        .dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| cursor_payload.as_ref().map(|c| c.dir.clone()))
        .unwrap_or_else(|| ".".to_string());
    let limit = request
        .limit
        .or(cursor_payload.as_ref().map(|c| c.limit).filter(|n| *n > 0))
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let max_chars = request
        .max_chars
        .or(cursor_payload
            .as_ref()
            .map(|c| c.max_chars)
            .filter(|n| *n > 0))
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(1, MAX_MAX_CHARS);
    let all = request
        .all
        .or(cursor_payload.as_ref().map(|c| Some(c.all)).unwrap_or(None))
        .unwrap_or(DEFAULT_ALL);
    let allow_secrets = request
        .allow_secrets
        .or(cursor_payload
            .as_ref()
            .map(|c| Some(c.allow_secrets))
            .unwrap_or(None))
        .unwrap_or(DEFAULT_ALLOW_SECRETS);
    let cursor_last_entry = cursor_payload.as_ref().map(|c| c.last_entry.as_str());

    let mut result = match compute_ls_result(
        &root,
        &root_display,
        &effective_dir,
        limit,
        max_chars,
        all,
        allow_secrets,
        cursor_last_entry,
    )
    .await
    {
        Ok(r) => r,
        Err(err) => {
            return Ok(internal_error_with_meta(
                format!("Failed to list directory: {err}"),
                meta_for_output.clone(),
            ));
        }
    };

    if let Err(err) = finalize_ls_budget(&mut result, max_chars) {
        return Ok(internal_error_with_meta(
            format!("Failed to finalize ls budget: {err}"),
            meta_for_output.clone(),
        ));
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
                    "dir": effective_dir,
                    "all": all,
                    "allow_secrets": allow_secrets,
                    "limit": limit,
                    "max_chars": max_chars,
                    "cursor": cursor,
                }),
                reason: "Continue ls pagination with the next cursor.".to_string(),
            }]);
        }
    }

    let mut doc = ContextDocBuilder::new();
    if response_mode != ResponseMode::Minimal {
        doc.push_root_fingerprint(meta_for_output.root_fingerprint);
    }
    if result.entries.is_empty() && !result.truncated {
        doc.push_answer("(empty)");
    } else if let Some(first) = result.entries.first() {
        doc.push_answer(first);
        for entry in result.entries.iter().skip(1) {
            doc.push_line(entry);
        }
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
        "ls",
    ))
}
