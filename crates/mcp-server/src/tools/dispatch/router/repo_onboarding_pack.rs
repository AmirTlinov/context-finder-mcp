use super::super::{
    compute_repo_onboarding_pack_result, AutoIndexPolicy, CallToolResult, Content,
    ContextFinderService, McpError, RepoOnboardingPackRequest, ResponseMode, ToolMeta,
};
use crate::tools::context_doc::ContextDocBuilder;

use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_request_with_meta,
    meta_for_request,
};
/// Repo onboarding pack (map + key docs slices + next actions).
pub(in crate::tools::dispatch) async fn repo_onboarding_pack(
    service: &ContextFinderService,
    request: RepoOnboardingPackRequest,
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
            return Ok(invalid_request_with_meta(message, meta, None, Vec::new()));
        }
    };
    let policy = AutoIndexPolicy::from_request(request.auto_index, request.auto_index_budget_ms);
    let meta = service.tool_meta_with_auto_index(&root, policy).await;
    let meta_for_output = if response_mode == ResponseMode::Minimal {
        ToolMeta {
            root_fingerprint: meta.root_fingerprint,
            ..ToolMeta::default()
        }
    } else {
        meta.clone()
    };
    let mut result = match compute_repo_onboarding_pack_result(&root, &root_display, &request).await
    {
        Ok(result) => result,
        Err(err) => {
            return Ok(internal_error_with_meta(
                format!("Error: {err:#}"),
                meta_for_output.clone(),
            ));
        }
    };
    result.meta = meta_for_output.clone();

    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!(
        "repo_onboarding_pack: dirs={} docs={}",
        result.map.directories.len(),
        result.docs.len()
    ));
    if response_mode != ResponseMode::Minimal {
        doc.push_root_fingerprint(meta_for_output.root_fingerprint);
    }
    doc.push_note("tree:");
    for dir in &result.map.directories {
        doc.push_line(&dir.path);
    }
    doc.push_blank();
    doc.push_note("docs:");
    for slice in &result.docs {
        doc.push_ref_header(&slice.file, slice.start_line, Some("doc"));
        doc.push_block_smart(&slice.content);
        doc.push_blank();
    }
    if result.docs.is_empty() {
        if let Some(reason) = result.docs_reason {
            doc.push_note(&format!("docs_reason={reason:?}"));
        }
    }
    if result.budget.truncated && !result.omitted_doc_paths.is_empty() {
        doc.push_note("omitted_doc_paths (not included):");
        let max_list = 10usize;
        for path in result.omitted_doc_paths.iter().take(max_list) {
            doc.push_line(path);
        }
        if result.omitted_doc_paths.len() > max_list {
            doc.push_note(&format!(
                "(+{} more omitted paths)",
                result.omitted_doc_paths.len().saturating_sub(max_list)
            ));
        }

        // Provide copy/paste runnable follow-ups to continue onboarding without guessing.
        let first = &result.omitted_doc_paths[0];
        let first_json = serde_json::to_string(first).unwrap_or_else(|_| "\"<invalid>\"".into());
        doc.push_note("next (narrow, copy/paste):");
        doc.push_note(&format!(
            "repo_onboarding_pack {{\"doc_paths\":[{first_json}],\"docs_limit\":1}}"
        ));
        if result.omitted_doc_paths.len() >= 2 {
            let second = &result.omitted_doc_paths[1];
            let second_json =
                serde_json::to_string(second).unwrap_or_else(|_| "\"<invalid>\"".into());
            doc.push_note(&format!(
                "repo_onboarding_pack {{\"doc_paths\":[{first_json},{second_json}],\"docs_limit\":2}}"
            ));
        }
        doc.push_blank();
    }
    if result.budget.truncated {
        if let Some(truncation) = result.budget.truncation.as_ref() {
            doc.push_note(&format!("truncated=true ({truncation:?})"));
        } else {
            doc.push_note("truncated=true");
        }
    }
    let output = CallToolResult::success(vec![Content::text(doc.finish())]);
    Ok(attach_structured_content(
        output,
        &result,
        meta_for_output,
        "repo_onboarding_pack",
    ))
}
