use super::super::router::cursor_alias::expand_cursor_alias;
use super::super::router::error::{
    attach_meta, invalid_request_with_root_context, meta_for_request,
};
use super::context::{build_context, ReadPackContext};
use super::cursors::trimmed_non_empty_str;
use super::intent_resolve::resolve_intent;
use super::{
    call_error, decode_cursor, CallToolResult, ContextFinderService, ReadPackIntent,
    ReadPackRequest, ResponseMode, DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS,
};
use crate::tools::dispatch::root::rel_path_string;
use crate::tools::dispatch::AutoIndexPolicy;
use context_indexer::{root_fingerprint, ToolMeta};
use serde_json::Value;
use std::path::Path;

pub(super) struct PreparedReadPack {
    pub(super) request: ReadPackRequest,
    pub(super) ctx: ReadPackContext,
    pub(super) intent: ReadPackIntent,
    pub(super) response_mode: ResponseMode,
    pub(super) timeout_ms: u64,
    pub(super) meta: ToolMeta,
    pub(super) meta_for_output: Option<ToolMeta>,
    pub(super) semantic_index_fresh: bool,
    pub(super) allow_secrets: bool,
}

pub(super) async fn prepare_read_pack(
    service: &ContextFinderService,
    mut request: ReadPackRequest,
) -> Result<PreparedReadPack, CallToolResult> {
    // Expand compact cursor aliases early so routing and cursor-only continuation work.
    // Without this, `resolve_intent` would attempt to decode a non-base64 cursor alias directly.
    if let Some(cursor) = request.cursor.as_deref() {
        match expand_cursor_alias(service, cursor).await {
            Ok(expanded) => request.cursor = Some(expanded),
            Err(message) => {
                let meta = meta_for_request(service, request.path.as_deref()).await;
                return Err(attach_meta(call_error("invalid_cursor", message), meta));
            }
        }
    }

    // Cursor-only continuation: if the caller didn't pass `path`, we can fall back to the cursor's
    // embedded root *only when the current session has no established root*.
    // This is a safety boundary for multi-agent / multi-project usage.
    if trimmed_non_empty_str(request.path.as_deref()).is_none() {
        if let Some(cursor) = request.cursor.as_deref() {
            if let Ok(value) = decode_cursor::<Value>(cursor) {
                if let Some(root) = value.get("root").and_then(Value::as_str) {
                    let cursor_root = root.trim();
                    if !cursor_root.is_empty() {
                        let session_root_display = { service.session.lock().await.root_display() };
                        if let Some(session_root_display) = session_root_display {
                            if session_root_display != cursor_root {
                                let message = "Invalid cursor: cursor refers to a different project root than the current session; call `root_set` to switch projects (or pass `path`)."
                                    .to_string();
                                let meta = ToolMeta {
                                    root_fingerprint: Some(root_fingerprint(&session_root_display)),
                                    ..ToolMeta::default()
                                };
                                return Err(attach_meta(
                                    call_error("invalid_cursor", message),
                                    meta,
                                ));
                            }
                        } else {
                            request.path = Some(cursor_root.to_string());
                        }
                    }
                }
            }
        }
    }

    // DX convenience: callers often pass `path` as a *subdirectory or file within the project*.
    // When the session already has a root, treat a relative `path` with no `file`/`file_pattern`
    // and no cursor as a file/file_pattern hint instead of switching the session root.
    let cursor_missing = trimmed_non_empty_str(request.cursor.as_deref()).is_none();
    let file_missing = trimmed_non_empty_str(request.file.as_deref()).is_none();
    let file_pattern_missing = trimmed_non_empty_str(request.file_pattern.as_deref()).is_none();
    if cursor_missing && file_missing && file_pattern_missing {
        if let Some(raw_path) = trimmed_non_empty_str(request.path.as_deref()) {
            let session_root = { service.session.lock().await.clone_root().map(|(r, _)| r) };
            if let Some(session_root) = session_root.as_ref() {
                let raw = Path::new(raw_path);
                if raw.is_absolute() {
                    if let Ok(canonical) = raw.canonicalize() {
                        if canonical.starts_with(session_root) {
                            if let Ok(rel) = canonical.strip_prefix(session_root) {
                                if let Some(rel) = rel_path_string(rel) {
                                    let is_file = std::fs::metadata(&canonical)
                                        .ok()
                                        .map(|meta| meta.is_file())
                                        .unwrap_or(false);
                                    if is_file {
                                        request.file = Some(rel);
                                    } else {
                                        let mut pattern = rel;
                                        if !pattern.ends_with('/') {
                                            pattern.push('/');
                                        }
                                        request.file_pattern = Some(pattern);
                                    }
                                    request.path = None;
                                }
                            }
                        }
                    }
                } else {
                    let normalized = raw_path.trim_start_matches("./");
                    if normalized == "." || normalized.is_empty() {
                        request.path = None;
                    } else {
                        let candidate = session_root.join(normalized);
                        let meta = std::fs::metadata(&candidate).ok();
                        let is_file = meta.as_ref().map(|m| m.is_file()).unwrap_or(false);
                        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                        if is_file {
                            request.file = Some(normalized.to_string());
                        } else {
                            let mut pattern = normalized.to_string();
                            if is_dir && !pattern.ends_with('/') {
                                pattern.push('/');
                            }
                            request.file_pattern = Some(pattern);
                        }
                        request.path = None;
                    }
                }
            }
        }
    }

    let mut hints: Vec<String> = Vec::new();
    if let Some(file) = request.file.as_deref() {
        hints.push(file.to_string());
    }
    if let Some(pattern) = request.file_pattern.as_deref() {
        if !pattern.trim().is_empty() {
            hints.push(pattern.to_string());
        }
    }
    let (root, root_display) = match service
        .resolve_root_with_hints_for_tool(request.path.as_deref(), &hints, "read_pack")
        .await
    {
        Ok(value) => value,
        Err(message) => {
            return Err(invalid_request_with_root_context(
                service,
                message,
                ToolMeta::default(),
                None,
                Vec::new(),
            )
            .await)
        }
    };
    let base_meta = service.tool_meta(&root).await;

    // Cursor-only continuation should preserve caller-selected budgets and response mode.
    if let Some(cursor) = request.cursor.as_deref() {
        match decode_cursor::<Value>(cursor) {
            Ok(value) => {
                if request.max_chars.is_none() {
                    if let Some(n) = value.get("max_chars").and_then(Value::as_u64) {
                        if n > 0 {
                            request.max_chars = Some(n as usize);
                        }
                    }
                }
                if request.response_mode.is_none() {
                    if let Some(mode_value) = value.get("response_mode") {
                        if let Ok(mode) = serde_json::from_value::<ResponseMode>(mode_value.clone())
                        {
                            request.response_mode = Some(mode);
                        }
                    }
                }
            }
            Err(err) => {
                return Err(attach_meta(
                    call_error("invalid_cursor", format!("Invalid cursor: {err}")),
                    base_meta.clone(),
                ))
            }
        }
    }

    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let ctx = match build_context(&request, root, root_display) {
        Ok(value) => value,
        Err(result) => return Err(attach_meta(result, base_meta.clone())),
    };
    let intent = match resolve_intent(&request) {
        Ok(value) => value,
        Err(result) => return Err(attach_meta(result, base_meta.clone())),
    };

    let timeout_ms = request
        .timeout_ms
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(1_000, MAX_TIMEOUT_MS);
    let meta = match intent {
        ReadPackIntent::Query => {
            service
                .tool_meta_with_auto_index(&ctx.root, AutoIndexPolicy::semantic_default())
                .await
        }
        _ => base_meta.clone(),
    };

    // Low-noise default: keep the response mostly project content.
    let provenance_meta = ToolMeta {
        root_fingerprint: meta.root_fingerprint,
        ..ToolMeta::default()
    };
    let meta_for_output = if response_mode == ResponseMode::Full {
        Some(meta.clone())
    } else {
        Some(provenance_meta)
    };

    let semantic_index_fresh = meta
        .index_state
        .as_ref()
        .is_some_and(|state| state.index.exists && !state.stale);
    let allow_secrets = request.allow_secrets.unwrap_or(false);

    Ok(PreparedReadPack {
        request,
        ctx,
        intent,
        response_mode,
        timeout_ms,
        meta,
        meta_for_output,
        semantic_index_fresh,
        allow_secrets,
    })
}
