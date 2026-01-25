use super::super::{apply_notebook_edit, CallToolResult, Content, ContextFinderService, McpError};
use crate::tools::context_doc::ContextDocBuilder;

use super::super::{NotebookEditRequest, ResponseMode, ToolMeta};
use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_request_with_root_context,
    meta_for_request,
};
use crate::tools::schemas::notebook_edit::NotebookEditResult;

/// Agent notebook: edit anchors/runbooks (durable, explicit writes).
pub(in crate::tools::dispatch) async fn notebook_edit(
    service: &ContextFinderService,
    request: NotebookEditRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = ResponseMode::Facts;
    let (root, _) = match service
        .resolve_root_no_daemon_touch_for_tool(request.path.as_deref(), "notebook_edit")
        .await
    {
        Ok(value) => value,
        Err(message) => {
            let meta = meta_for_request(service, request.path.as_deref()).await;
            return Ok(
                invalid_request_with_root_context(service, message, meta, None, Vec::new()).await,
            );
        }
    };

    let meta_for_output = if response_mode == ResponseMode::Minimal {
        ToolMeta::default()
    } else {
        meta_for_request(service, request.path.as_deref()).await
    };

    let summary = match apply_notebook_edit(&root, &request).await {
        Ok(value) => value,
        Err(err) => {
            return Ok(internal_error_with_meta(
                format!("{err:#}"),
                meta_for_output,
            ));
        }
    };

    let result = NotebookEditResult {
        version: 1,
        applied_ops: summary.applied_ops,
        anchors: summary.anchors,
        runbooks: summary.runbooks,
        touched_anchor_ids: summary.touched_anchor_ids.clone(),
        touched_runbook_ids: summary.touched_runbook_ids.clone(),
        next_actions: Vec::new(),
        meta: meta_for_output.clone(),
    };

    let mut doc = ContextDocBuilder::new();
    doc.push_answer("notebook_edit");
    doc.push_root_fingerprint(meta_for_output.root_fingerprint);
    doc.push_note(&format!("applied_ops={}", summary.applied_ops));
    doc.push_note(&format!("anchors={}", summary.anchors));
    doc.push_note(&format!("runbooks={}", summary.runbooks));

    if !summary.touched_anchor_ids.is_empty() {
        doc.push_blank();
        doc.push_note("touched_anchors:");
        for id in &summary.touched_anchor_ids {
            doc.push_line(&format!(" - {id}"));
        }
    }

    if !summary.touched_runbook_ids.is_empty() {
        doc.push_blank();
        doc.push_note("touched_runbooks:");
        for id in &summary.touched_runbook_ids {
            doc.push_line(&format!(" - {id}"));
        }
    }

    let text = doc.finish();
    let call_result = CallToolResult::success(vec![Content::text(text)]);
    Ok(attach_structured_content(
        call_result,
        &result,
        meta_for_output,
        "notebook_edit",
    ))
}
