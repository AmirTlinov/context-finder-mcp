use super::super::{
    compute_notebook_suggest_result, CallToolResult, Content, ContextFinderService, McpError,
    NotebookSuggestRequest, ResponseMode, ToolMeta,
};
use crate::tools::context_doc::ContextDocBuilder;

use super::error::{attach_structured_content, internal_error_with_meta, meta_for_request};

/// Notebook suggestions: propose anchors + runbooks (read-only; evidence-backed).
pub(in crate::tools::dispatch) async fn notebook_suggest(
    service: &ContextFinderService,
    request: NotebookSuggestRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);

    let (root, root_display) = match service
        .resolve_root_no_daemon_touch(request.path.as_deref())
        .await
    {
        Ok(value) => value,
        Err(message) => {
            let meta = if response_mode == ResponseMode::Minimal {
                ToolMeta::default()
            } else {
                meta_for_request(service, request.path.as_deref()).await
            };
            return Ok(super::error::invalid_request_with_meta(
                message,
                meta,
                None,
                Vec::new(),
            ));
        }
    };

    let meta_for_output = if response_mode == ResponseMode::Minimal {
        ToolMeta {
            root_fingerprint: Some(context_indexer::root_fingerprint(&root_display)),
            ..ToolMeta::default()
        }
    } else {
        meta_for_request(service, request.path.as_deref()).await
    };

    let mut result = match compute_notebook_suggest_result(&root, &root_display, &request).await {
        Ok(value) => value,
        Err(err) => {
            return Ok(internal_error_with_meta(
                format!("Error: {err:#}"),
                meta_for_output.clone(),
            ));
        }
    };
    result.meta = meta_for_output.clone();

    let mut doc = ContextDocBuilder::new();
    doc.push_answer("notebook_suggest");
    doc.push_root_fingerprint(meta_for_output.root_fingerprint);

    doc.push_note(&format!("repo_id={}", result.repo_id));
    doc.push_note(&format!("query={:?}", result.query));
    doc.push_note(&format!(
        "suggested_anchors={} suggested_runbooks={}",
        result.anchors.len(),
        result.runbooks.len()
    ));

    if response_mode != ResponseMode::Minimal {
        doc.push_blank();
        doc.push_note("anchors:");
        for a in &result.anchors {
            let ev = a.evidence.first();
            let ev_note = ev
                .map(|p| format!("ev={}:{}-{}", p.file, p.start_line, p.end_line))
                .unwrap_or_else(|| "ev=<missing>".to_string());
            doc.push_note(&format!(
                "anchor id={} kind={:?} label={:?} {}",
                a.id, a.kind, a.label, ev_note
            ));
        }

        doc.push_blank();
        doc.push_note("runbooks:");
        for rb in &result.runbooks {
            doc.push_note(&format!(
                "runbook id={} title={:?} sections={}",
                rb.id,
                rb.title,
                rb.sections.len()
            ));
        }
    }

    if response_mode == ResponseMode::Full && !result.next_actions.is_empty() {
        doc.push_blank();
        doc.push_note("next_actions:");
        for action in &result.next_actions {
            let mut args = serde_json::to_string(&action.args).unwrap_or_else(|_| "{}".to_string());
            if args.len() > 400 {
                args.truncate(400);
                args.push('…');
            }
            doc.push_note(&format!(
                "next_action tool={} args={} reason={}",
                action.tool, args, action.reason
            ));
        }
    }

    let (text, bounded_truncated) = doc.finish_bounded(result.budget.max_chars);
    result.budget.used_chars = text.chars().count();
    result.budget.truncated = result.budget.truncated || bounded_truncated;

    let call_result = CallToolResult::success(vec![Content::text(text)]);
    Ok(attach_structured_content(
        call_result,
        &result,
        meta_for_output,
        "notebook_suggest",
    ))
}
