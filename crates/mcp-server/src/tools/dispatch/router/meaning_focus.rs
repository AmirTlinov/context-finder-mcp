use super::super::{
    compute_meaning_focus_result, AutoIndexPolicy, CallToolResult, Content, ContextFinderService,
    McpError, MeaningFocusRequest, ResponseMode, ToolMeta,
};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::cpv1::cpv1_coverage;
use crate::tools::schemas::meaning_focus::MeaningFocusOutputFormat;
use base64::{engine::general_purpose::STANDARD, Engine as _};

use super::error::{attach_structured_content, invalid_request_with_meta, meta_for_request};

/// Meaning-first focus (semantic zoom): scoped CP + evidence pointers.
pub(in crate::tools::dispatch) async fn meaning_focus(
    service: &ContextFinderService,
    request: MeaningFocusRequest,
) -> Result<CallToolResult, McpError> {
    let output_format = request
        .output_format
        .unwrap_or(MeaningFocusOutputFormat::Context);
    let want_context = matches!(
        output_format,
        MeaningFocusOutputFormat::Context | MeaningFocusOutputFormat::ContextAndDiagram
    );
    let want_diagram = matches!(
        output_format,
        MeaningFocusOutputFormat::Diagram | MeaningFocusOutputFormat::ContextAndDiagram
    );

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

    let mut result = match compute_meaning_focus_result(&root, &root_display, &request).await {
        Ok(result) => result,
        Err(err) => {
            return Ok(invalid_request_with_meta(
                format!("Invalid meaning_focus request: {err:#}"),
                meta_for_output.clone(),
                None,
                Vec::new(),
            ));
        }
    };
    result.meta = meta_for_output.clone();

    let mut contents: Vec<Content> = Vec::new();
    if want_context {
        let mut doc = ContextDocBuilder::new();
        doc.push_answer("meaning_focus");
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
        if response_mode != ResponseMode::Minimal {
            let cov = cpv1_coverage(&result.pack);
            doc.push_note(&format!(
                "coverage: anchors_ev={}/{} steps_ev={}/{} ev={}",
                cov.anchors_with_evidence,
                cov.anchors_total,
                cov.steps_with_evidence,
                cov.steps_total,
                cov.evidence_total
            ));
        }
        if response_mode == ResponseMode::Full && !result.next_actions.is_empty() {
            doc.push_blank();
            doc.push_note("next_actions:");
            for action in &result.next_actions {
                let mut args =
                    serde_json::to_string(&action.args).unwrap_or_else(|_| "{}".to_string());
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
        contents.push(Content::text(doc.finish()));
    }
    if want_diagram {
        let svg =
            crate::tools::meaning_diagram::render_meaning_pack_svg(&result.pack, &result.query, 24);
        contents.push(Content::image(
            STANDARD.encode(svg.as_bytes()),
            "image/svg+xml",
        ));
    }

    let output = CallToolResult::success(contents);
    Ok(attach_structured_content(
        output,
        &result,
        meta_for_output,
        "meaning_focus",
    ))
}
