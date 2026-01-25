use super::super::{
    compute_notebook_pack_result, CallToolResult, Content, ContextFinderService, McpError,
    NotebookPackRequest, ResponseMode, ToolMeta,
};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::notebook_store::staleness_for_anchor;

use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_request_with_root_context,
    meta_for_request,
};

/// Agent notebook: list anchors + runbooks (durable, cross-session).
pub(in crate::tools::dispatch) async fn notebook_pack(
    service: &ContextFinderService,
    request: NotebookPackRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);

    let (root, _) = match service
        .resolve_root_no_daemon_touch_for_tool(request.path.as_deref(), "notebook_pack")
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

    let mut result = match compute_notebook_pack_result(&root, &request).await {
        Ok(value) => value,
        Err(err) => {
            let meta = if response_mode == ResponseMode::Minimal {
                ToolMeta::default()
            } else {
                meta_for_request(service, request.path.as_deref()).await
            };
            return Ok(internal_error_with_meta(format!("{err:#}"), meta));
        }
    };

    let meta_for_output = if response_mode == ResponseMode::Minimal {
        ToolMeta::default()
    } else {
        meta_for_request(service, request.path.as_deref()).await
    };
    result.meta = meta_for_output.clone();

    let mut doc = ContextDocBuilder::new();
    doc.push_answer("notebook_pack");
    doc.push_root_fingerprint(meta_for_output.root_fingerprint);
    doc.push_note(&format!("repo_id={}", result.repo_id));

    doc.push_note("anchors:");
    if result.anchors.is_empty() {
        doc.push_line(" (none)");
    } else {
        for a in &result.anchors {
            if response_mode == ResponseMode::Minimal {
                doc.push_line(&format!(
                    " - {:?} {} (id={}, ev={})",
                    a.kind,
                    a.label,
                    a.id,
                    a.evidence.len()
                ));
                continue;
            }
            let stale = staleness_for_anchor(&root, a).ok();
            match stale {
                Some((total, stale)) => {
                    doc.push_line(&format!(
                        " - {:?} {} (id={}, ev={}, stale={}/{})",
                        a.kind,
                        a.label,
                        a.id,
                        a.evidence.len(),
                        stale,
                        total
                    ));
                }
                None => {
                    doc.push_line(&format!(
                        " - {:?} {} (id={}, ev={}, stale=unknown)",
                        a.kind,
                        a.label,
                        a.id,
                        a.evidence.len()
                    ));
                }
            }
        }
    }

    doc.push_blank();
    doc.push_note("runbooks:");
    if result.runbooks.is_empty() {
        doc.push_line(" (none)");
    } else {
        for rb in &result.runbooks {
            doc.push_line(&format!(
                " - {} (id={}, sections={})",
                rb.title,
                rb.id,
                rb.sections.len()
            ));
        }
    }

    if result.budget.truncated {
        doc.push_blank();
        doc.push_note("truncated=true");
    }

    let max_chars = request.max_chars.unwrap_or(result.budget.max_chars);
    let (text, doc_truncated) = doc.finish_bounded(max_chars);
    result.budget.max_chars = max_chars;
    result.budget.used_chars = text.chars().count();
    result.budget.truncated = doc_truncated;
    let output = CallToolResult::success(vec![Content::text(text)]);
    Ok(attach_structured_content(
        output,
        &result,
        meta_for_output,
        "notebook_pack",
    ))
}
