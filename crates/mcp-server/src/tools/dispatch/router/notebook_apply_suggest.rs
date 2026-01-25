use super::super::{
    apply_notebook_apply_suggest, CallToolResult, Content, ContextFinderService, McpError,
    NotebookApplySuggestRequest, ResponseMode, ToolMeta,
};
use crate::tools::context_doc::ContextDocBuilder;

use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_request_with_root_context,
    meta_for_request,
};
use crate::tools::schemas::notebook_apply_suggest::{
    NotebookApplySuggestChangeAction, NotebookApplySuggestChangeKind, NotebookApplySuggestResult,
    NotebookApplySuggestSkipReason,
};

fn request_path(request: &NotebookApplySuggestRequest) -> Option<&str> {
    match request {
        NotebookApplySuggestRequest::Preview { path, .. } => path.as_deref(),
        NotebookApplySuggestRequest::Apply { path, .. } => path.as_deref(),
        NotebookApplySuggestRequest::Rollback { path, .. } => path.as_deref(),
    }
}

/// Notebook apply: one-click preview/apply/rollback for notebook_suggest output.
pub(in crate::tools::dispatch) async fn notebook_apply_suggest(
    service: &ContextFinderService,
    request: NotebookApplySuggestRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = ResponseMode::Facts;
    let path = request_path(&request);

    let (root, _) = match service
        .resolve_root_no_daemon_touch_for_tool(path, "notebook_apply_suggest")
        .await
    {
        Ok(value) => value,
        Err(message) => {
            let meta = if response_mode == ResponseMode::Minimal {
                ToolMeta::default()
            } else {
                meta_for_request(service, path).await
            };
            return Ok(
                invalid_request_with_root_context(service, message, meta, None, Vec::new()).await,
            );
        }
    };

    let meta_for_output = if response_mode == ResponseMode::Minimal {
        ToolMeta::default()
    } else {
        meta_for_request(service, path).await
    };

    let outcome = match apply_notebook_apply_suggest(&root, &request).await {
        Ok(value) => value,
        Err(err) => {
            return Ok(internal_error_with_meta(
                format!("{err:#}"),
                meta_for_output.clone(),
            ));
        }
    };

    let result = NotebookApplySuggestResult {
        version: 1,
        mode: outcome.mode,
        repo_id: outcome.repo_id.clone(),
        scope: outcome.scope,
        backup_id: outcome.backup_id.clone(),
        warnings: outcome.warnings.clone(),
        summary: outcome.summary.clone(),
        next_actions: Vec::new(),
        meta: meta_for_output.clone(),
    };

    let mut doc = ContextDocBuilder::new();
    doc.push_answer("notebook_apply_suggest");
    doc.push_root_fingerprint(meta_for_output.root_fingerprint);
    doc.push_note(&format!("mode={:?}", outcome.mode));
    doc.push_note(&format!("repo_id={}", outcome.repo_id));
    doc.push_note(&format!("scope={:?}", outcome.scope));

    if let Some(backup_id) = outcome.backup_id.as_deref() {
        doc.push_note(&format!("backup_id={backup_id}"));
    }

    doc.push_note(&format!(
        "anchors: {} -> {} (new={} updated={} skipped={})",
        outcome.summary.anchors_before,
        outcome.summary.anchors_after,
        outcome.summary.new_anchors,
        outcome.summary.updated_anchors,
        outcome.summary.skipped_anchors
    ));
    doc.push_note(&format!(
        "runbooks: {} -> {} (new={} updated={} skipped={})",
        outcome.summary.runbooks_before,
        outcome.summary.runbooks_after,
        outcome.summary.new_runbooks,
        outcome.summary.updated_runbooks,
        outcome.summary.skipped_runbooks
    ));

    if outcome.mode
        == crate::tools::schemas::notebook_apply_suggest::NotebookApplySuggestMode::Preview
        && !outcome.summary.changes.is_empty()
    {
        doc.push_blank();
        doc.push_note("changes (preview):");
        let mut shown = 0usize;
        for ch in outcome.summary.changes.iter().take(20) {
            shown += 1;
            let kind = match ch.kind {
                NotebookApplySuggestChangeKind::Anchor => "anchor",
                NotebookApplySuggestChangeKind::Runbook => "runbook",
            };
            let action = match ch.action {
                NotebookApplySuggestChangeAction::Create => "create",
                NotebookApplySuggestChangeAction::Update => "update",
                NotebookApplySuggestChangeAction::Skip => "skip",
            };
            let reason = match ch.reason {
                Some(NotebookApplySuggestSkipReason::NotManaged) => Some("not_managed"),
                Some(NotebookApplySuggestSkipReason::ManualModified) => Some("manual_modified"),
                None => None,
            };
            let hint = ch.hint.as_deref().unwrap_or("");
            let mut line = format!(" - {kind} {}: {action}", ch.id);
            if let Some(reason) = reason {
                line.push_str(&format!(" ({reason})"));
            }
            if !hint.is_empty() {
                line.push_str(&format!(" — {hint}"));
            }
            doc.push_line(&line);
        }
        if outcome.summary.changes.len() > shown {
            doc.push_line(&format!(
                " - … (showing {shown} of {})",
                outcome.summary.changes.len()
            ));
        }
    }

    if !outcome.warnings.is_empty() {
        doc.push_blank();
        doc.push_note("warnings:");
        for w in &outcome.warnings {
            doc.push_line(&format!(" - {w}"));
        }
    }

    let text = doc.finish();
    let call_result = CallToolResult::success(vec![Content::text(text)]);
    Ok(attach_structured_content(
        call_result,
        &result,
        meta_for_output,
        "notebook_apply_suggest",
    ))
}
