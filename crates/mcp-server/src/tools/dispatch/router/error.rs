use super::super::{CallToolResult, Content, ContextFinderService};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::dispatch::root::root_context_details;
use context_indexer::ToolMeta;
use context_protocol::{ErrorEnvelope, ToolNextAction};
use rmcp::model::RawContent;
use serde::Serialize;
use serde_json::json;

fn render_details_value(value: &serde_json::Value, max_len: usize) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => {
            let mut out = s
                .split_whitespace()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if out.len() > max_len {
                out.truncate(max_len);
                out.push('…');
            }
            out
        }
        serde_json::Value::Array(values) => format!("<array len={}>", values.len()),
        serde_json::Value::Object(values) => format!("<object keys={}>", values.len()),
    }
}

fn render_details_notes(details: &serde_json::Value) -> Vec<String> {
    const MAX_LINES: usize = 8;
    const MAX_VALUE_CHARS: usize = 200;

    match details {
        serde_json::Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();

            let mut out = Vec::new();
            for key in keys.into_iter().take(MAX_LINES) {
                let value = map.get(key).expect("key present");
                out.push(format!(
                    "details.{key}={}",
                    render_details_value(value, MAX_VALUE_CHARS)
                ));
            }
            if map.len() > MAX_LINES {
                out.push(format!("details.more_keys={}", map.len() - MAX_LINES));
            }
            out
        }
        other => vec![format!("details={}", render_details_value(other, 400))],
    }
}

pub(in crate::tools::dispatch) fn tool_error_envelope(error: ErrorEnvelope) -> CallToolResult {
    tool_error_envelope_with_meta(error, ToolMeta::default())
}

pub(in crate::tools::dispatch) fn tool_error_envelope_with_meta(
    error: ErrorEnvelope,
    meta: ToolMeta,
) -> CallToolResult {
    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!("error: {}", error.code));
    doc.push_note(&error.message);
    if let Some(hint) = error.hint.as_deref() {
        if !hint.trim().is_empty() {
            doc.push_note(&format!("hint: {hint}"));
        }
    }
    if let Some(details) = error.details.as_ref() {
        for line in render_details_notes(details) {
            doc.push_note(&line);
        }
    }
    doc.push_root_fingerprint(meta.root_fingerprint);
    for action in &error.next_actions {
        doc.push_note(&format!("next: {} ({})", action.tool, action.reason));
    }

    let mut result = CallToolResult::error(vec![Content::text(doc.finish())]);
    result.structured_content = Some(json!({ "error": error, "meta": meta }));
    result
}

pub(in crate::tools::dispatch) fn tool_error(
    code: &'static str,
    message: impl Into<String>,
) -> CallToolResult {
    tool_error_envelope(ErrorEnvelope {
        code: code.to_string(),
        message: message.into(),
        details: None,
        hint: None,
        next_actions: Vec::new(),
    })
}

pub(in crate::tools::dispatch) fn invalid_request(message: impl Into<String>) -> CallToolResult {
    tool_error("invalid_request", message)
}

pub(in crate::tools::dispatch) fn invalid_cursor(message: impl Into<String>) -> CallToolResult {
    tool_error("invalid_cursor", message)
}

pub(in crate::tools::dispatch) fn internal_error(message: impl Into<String>) -> CallToolResult {
    tool_error("internal", message)
}

pub(in crate::tools::dispatch) fn invalid_cursor_with_meta(
    message: impl Into<String>,
    meta: ToolMeta,
) -> CallToolResult {
    tool_error_envelope_with_meta(
        ErrorEnvelope {
            code: "invalid_cursor".to_string(),
            message: message.into(),
            details: None,
            hint: None,
            next_actions: Vec::new(),
        },
        meta,
    )
}

pub(in crate::tools::dispatch) fn invalid_cursor_with_meta_details(
    message: impl Into<String>,
    meta: ToolMeta,
    details: serde_json::Value,
) -> CallToolResult {
    tool_error_envelope_with_meta(
        ErrorEnvelope {
            code: "invalid_cursor".to_string(),
            message: message.into(),
            details: Some(details),
            hint: None,
            next_actions: Vec::new(),
        },
        meta,
    )
}

pub(in crate::tools::dispatch) fn cursor_mismatch_with_meta_details(
    message: impl Into<String>,
    meta: ToolMeta,
    details: serde_json::Value,
    hint: Option<String>,
    next_actions: Vec<ToolNextAction>,
) -> CallToolResult {
    tool_error_envelope_with_meta(
        ErrorEnvelope {
            code: "cursor_mismatch".to_string(),
            message: message.into(),
            details: Some(details),
            hint,
            next_actions,
        },
        meta,
    )
}

pub(in crate::tools::dispatch) fn invalid_request_with_meta(
    message: impl Into<String>,
    meta: ToolMeta,
    hint: Option<String>,
    next_actions: Vec<ToolNextAction>,
) -> CallToolResult {
    tool_error_envelope_with_meta(
        ErrorEnvelope {
            code: "invalid_request".to_string(),
            message: message.into(),
            details: None,
            hint,
            next_actions,
        },
        meta,
    )
}

pub(in crate::tools::dispatch) fn invalid_request_with_meta_details(
    message: impl Into<String>,
    meta: ToolMeta,
    details: serde_json::Value,
    hint: Option<String>,
    next_actions: Vec<ToolNextAction>,
) -> CallToolResult {
    tool_error_envelope_with_meta(
        ErrorEnvelope {
            code: "invalid_request".to_string(),
            message: message.into(),
            details: Some(details),
            hint,
            next_actions,
        },
        meta,
    )
}

pub(in crate::tools::dispatch) async fn invalid_request_with_root_context(
    service: &ContextFinderService,
    message: impl Into<String>,
    meta: ToolMeta,
    hint: Option<String>,
    next_actions: Vec<ToolNextAction>,
) -> CallToolResult {
    let message = message.into();
    if !needs_root_context(&message) {
        return invalid_request_with_meta(message, meta, hint, next_actions);
    }
    let root_context = root_context_details(service).await;
    if root_context.is_null() {
        return invalid_request_with_meta(message, meta, hint, next_actions);
    }
    invalid_request_with_meta_details(
        message,
        meta,
        json!({ "root_context": root_context }),
        hint,
        next_actions,
    )
}

fn needs_root_context(message: &str) -> bool {
    message.starts_with("Invalid path") || message.starts_with("Missing project root")
}

pub(in crate::tools::dispatch) fn internal_error_with_meta(
    message: impl Into<String>,
    meta: ToolMeta,
) -> CallToolResult {
    tool_error_envelope_with_meta(
        ErrorEnvelope {
            code: "internal".to_string(),
            message: message.into(),
            details: None,
            hint: None,
            next_actions: Vec::new(),
        },
        meta,
    )
}

pub(in crate::tools::dispatch) fn attach_structured_content<T: Serialize>(
    mut result: CallToolResult,
    payload: &T,
    meta: ToolMeta,
    tool: &'static str,
) -> CallToolResult {
    match serde_json::to_value(payload) {
        Ok(value) => {
            result.structured_content = Some(value);
            result
        }
        Err(err) => internal_error_with_meta(
            format!("Error: failed to serialize {tool} structured_content ({err})"),
            meta,
        ),
    }
}

pub(in crate::tools::dispatch) fn invalid_request_with(
    message: impl Into<String>,
    hint: Option<String>,
    next_actions: Vec<ToolNextAction>,
) -> CallToolResult {
    tool_error_envelope(ErrorEnvelope {
        code: "invalid_request".to_string(),
        message: message.into(),
        details: None,
        hint,
        next_actions,
    })
}

pub(in crate::tools::dispatch) fn attach_meta(
    mut result: CallToolResult,
    meta: ToolMeta,
) -> CallToolResult {
    let value = result.structured_content.get_or_insert_with(|| json!({}));
    if let Some(obj) = value.as_object_mut() {
        obj.insert("meta".to_string(), json!(meta.clone()));
    }

    // Some routers build an error envelope first, then attach meta once the root is known.
    // Keep error text debuggable even when the client UI hides structured output.
    if result.is_error == Some(true) {
        if let Some(root_fingerprint) = meta.root_fingerprint {
            let needs_note = result
                .content
                .first()
                .and_then(|c| c.as_text())
                .is_some_and(|t| !t.text.contains("root_fingerprint="));
            if needs_note {
                if let Some(first) = result.content.first_mut() {
                    if let RawContent::Text(text) = &mut first.raw {
                        if !text.text.ends_with('\n') {
                            text.text.push('\n');
                        }
                        text.text
                            .push_str(&format!("N: root_fingerprint={root_fingerprint}\n"));
                    }
                }
            }
        }
    }
    result
}

pub(in crate::tools::dispatch) async fn meta_for_request(
    service: &ContextFinderService,
    path: Option<&str>,
) -> ToolMeta {
    match resolve_root_for_meta(service, path).await {
        Some(root) => service.tool_meta(&root).await,
        None => ToolMeta::default(),
    }
}

async fn resolve_root_for_meta(
    service: &ContextFinderService,
    path: Option<&str>,
) -> Option<std::path::PathBuf> {
    service
        .resolve_root_no_daemon_touch(path)
        .await
        .ok()
        .map(|(root, _)| root)
}
