use super::super::{
    compute_evidence_fetch_result, CallToolResult, Content, ContextFinderService,
    EvidenceFetchRequest, McpError, ResponseMode, ToolMeta,
};
use crate::tools::context_doc::ContextDocBuilder;

use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_request_with_root_context,
    meta_for_request,
};

/// Evidence fetch (verbatim) for one or more evidence pointers.
pub(in crate::tools::dispatch) async fn evidence_fetch(
    service: &ContextFinderService,
    request: EvidenceFetchRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let (root, _root_display) = match service
        .resolve_root_no_daemon_touch_for_tool(request.path.as_deref(), "evidence_fetch")
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

    let meta_for_output = if response_mode == ResponseMode::Minimal {
        let meta = meta_for_request(service, request.path.as_deref()).await;
        ToolMeta {
            root_fingerprint: meta.root_fingerprint,
            ..ToolMeta::default()
        }
    } else {
        meta_for_request(service, request.path.as_deref()).await
    };

    let mut result = match compute_evidence_fetch_result(&root, &request).await {
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
    doc.push_answer(&format!("evidence_fetch: items={}", result.items.len()));
    if response_mode != ResponseMode::Minimal {
        doc.push_root_fingerprint(meta_for_output.root_fingerprint);
    }
    for item in &result.items {
        if response_mode == ResponseMode::Minimal {
            doc.push_line(&format!(
                "-- {}:{}–{} --",
                item.evidence.file, item.evidence.start_line, item.evidence.end_line
            ));
        } else {
            doc.push_ref_header(
                &item.evidence.file,
                item.evidence.start_line,
                Some("evidence"),
            );
            if let Some(hash) = item.evidence.source_hash.as_deref() {
                if !hash.trim().is_empty() {
                    doc.push_note(&format!("source_hash={hash}"));
                }
            }
            if item.stale {
                doc.push_note("stale=true");
            }
        }
        doc.push_block_smart(&item.content);
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
        "evidence_fetch",
    ))
}
