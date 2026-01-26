use super::super::super::router::cursor_alias::expand_cursor_alias;
use super::super::super::router::error::{attach_meta, meta_for_request};
use super::super::cursors::trimmed_non_empty_str;
use super::super::{
    call_error, decode_cursor, CallToolResult, ContextFinderService, ReadPackRequest, ResponseMode,
};
use context_indexer::{root_fingerprint, ToolMeta};
use serde_json::Value;

pub(super) async fn expand_cursor_aliases(
    service: &ContextFinderService,
    request: &mut ReadPackRequest,
) -> Result<(), CallToolResult> {
    if let Some(cursor) = request.cursor.as_deref() {
        match expand_cursor_alias(service, cursor).await {
            Ok(expanded) => request.cursor = Some(expanded),
            Err(message) => {
                let meta = meta_for_request(service, request.path.as_deref()).await;
                return Err(attach_meta(call_error("invalid_cursor", message), meta));
            }
        }
    }
    Ok(())
}

pub(super) async fn apply_cursor_root_fallback(
    service: &ContextFinderService,
    request: &mut ReadPackRequest,
) -> Result<(), CallToolResult> {
    if trimmed_non_empty_str(request.path.as_deref()).is_some() {
        return Ok(());
    }
    let Some(cursor) = request.cursor.as_deref() else {
        return Ok(());
    };
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
                        return Err(attach_meta(call_error("invalid_cursor", message), meta));
                    }
                } else {
                    request.path = Some(cursor_root.to_string());
                }
            }
        }
    }
    Ok(())
}

pub(super) fn apply_cursor_overrides(
    request: &mut ReadPackRequest,
    base_meta: &ToolMeta,
) -> Result<(), CallToolResult> {
    let Some(cursor) = request.cursor.as_deref() else {
        return Ok(());
    };
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
                    if let Ok(mode) = serde_json::from_value::<ResponseMode>(mode_value.clone()) {
                        request.response_mode = Some(mode);
                    }
                }
            }
            Ok(())
        }
        Err(err) => Err(attach_meta(
            call_error("invalid_cursor", format!("Invalid cursor: {err}")),
            base_meta.clone(),
        )),
    }
}
