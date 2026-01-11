use super::super::{
    compute_meaning_pack_result, AutoIndexPolicy, CallToolResult, Content, ContextFinderService,
    McpError, MeaningPackRequest, ResponseMode, ToolMeta,
};
use crate::tools::context_doc::ContextDocBuilder;

use super::error::{attach_structured_content, internal_error_with_meta, meta_for_request};

/// Meaning-first pack (facts-only CP + evidence pointers).
pub(in crate::tools::dispatch) async fn meaning_pack(
    service: &ContextFinderService,
    request: MeaningPackRequest,
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

    let mut result = match compute_meaning_pack_result(&root, &root_display, &request).await {
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
    doc.push_answer("meaning_pack");
    doc.push_root_fingerprint(meta_for_output.root_fingerprint);
    doc.push_note("pack:");
    doc.push_block_smart(&result.pack);
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
        "meaning_pack",
    ))
}
