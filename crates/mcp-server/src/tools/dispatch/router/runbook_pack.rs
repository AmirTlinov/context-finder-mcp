use super::super::{
    compute_runbook_pack_result, CallToolResult, Content, ContextFinderService, McpError,
    ResponseMode, RunbookPackRequest, ToolMeta,
};
use super::cursor_alias::{compact_cursor_alias, expand_cursor_alias};
use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_request_with_root_context,
    meta_for_request,
};
use crate::tools::context_doc::ContextDocBuilder;

/// Runbook runner: returns TOC by default; expand a section on demand (cursor-based).
pub(in crate::tools::dispatch) async fn runbook_pack(
    service: &ContextFinderService,
    mut request: RunbookPackRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);

    if let Some(cursor) = request.cursor.as_deref() {
        match expand_cursor_alias(service, cursor).await {
            Ok(expanded) => request.cursor = Some(expanded),
            Err(message) => {
                let meta = if response_mode == ResponseMode::Minimal {
                    ToolMeta::default()
                } else {
                    meta_for_request(service, request.path.as_deref()).await
                };
                return Ok(super::error::invalid_cursor_with_meta(message, meta));
            }
        }
    }

    let (root, root_display) = match service
        .resolve_root_no_daemon_touch_for_tool(request.path.as_deref(), "runbook_pack")
        .await
    {
        Ok(value) => value,
        Err(message) => {
            let meta = if response_mode == ResponseMode::Minimal {
                ToolMeta::default()
            } else {
                meta_for_request(service, request.path.as_deref()).await
            };
            return Ok(
                invalid_request_with_root_context(service, message, meta, None, Vec::new()).await,
            );
        }
    };

    let mut result = match compute_runbook_pack_result(
        &root,
        &root_display,
        &request,
        request.cursor.as_deref(),
    )
    .await
    {
        Ok(value) => value,
        Err(err) => {
            let meta = if response_mode == ResponseMode::Minimal {
                ToolMeta::default()
            } else {
                meta_for_request(service, request.path.as_deref()).await
            };
            let message = format!("{err:#}");
            if message.starts_with("Invalid cursor:") {
                return Ok(super::error::invalid_cursor_with_meta(message, meta));
            }
            return Ok(internal_error_with_meta(message, meta));
        }
    };

    let meta_for_output = if response_mode == ResponseMode::Minimal {
        ToolMeta::default()
    } else {
        meta_for_request(service, request.path.as_deref()).await
    };
    result.meta = meta_for_output.clone();

    if let Some(ref mut expanded) = result.expanded {
        if let Some(cursor) = expanded.next_cursor.take() {
            expanded.next_cursor = Some(compact_cursor_alias(service, cursor).await);
        }
    }

    let mut doc = ContextDocBuilder::new();
    doc.push_answer("runbook_pack");
    doc.push_root_fingerprint(meta_for_output.root_fingerprint);
    doc.push_note(&format!(
        "runbook: {} (id={})",
        result.runbook_title, result.runbook_id
    ));
    doc.push_note(&format!("mode={}", result.mode));

    doc.push_blank();
    doc.push_note("toc:");
    if result.toc.is_empty() {
        doc.push_line(" (none)");
    } else {
        for item in &result.toc {
            doc.push_line(&format!(
                " - {} [{}] {} status={} stale={}/{}",
                item.id, item.kind, item.title, item.status, item.stale_items, item.total_items
            ));
        }
    }

    if let Some(expanded) = &result.expanded {
        doc.push_blank();
        doc.push_note(&format!("section={}:", expanded.section_id));
        doc.push_block_smart(&expanded.content);
        if expanded.truncated {
            if let Some(cursor) = expanded.next_cursor.as_deref() {
                doc.push_cursor(cursor);
            }
        }
    }

    let max_chars = request.max_chars.unwrap_or(result.budget.max_chars);
    let (text, doc_truncated) = doc.finish_bounded(max_chars);
    let expanded_truncated = result
        .expanded
        .as_ref()
        .map(|e| e.truncated)
        .unwrap_or(false);
    result.budget.max_chars = max_chars;
    result.budget.used_chars = text.chars().count();
    result.budget.truncated = doc_truncated || expanded_truncated;
    let output = CallToolResult::success(vec![Content::text(text)]);
    Ok(attach_structured_content(
        output,
        &result,
        meta_for_output,
        "runbook_pack",
    ))
}
