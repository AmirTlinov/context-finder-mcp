use super::super::super::{CallToolResult, Content, ResponseMode};
use super::super::error::internal_error_with_meta;
use super::inputs::ContextPackInputs;
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::schemas::ToolNextAction;
use context_search::ContextPackOutput;

pub(super) fn build_retry_action(
    root_display: &str,
    query: &str,
    inputs: &ContextPackInputs,
    output: &ContextPackOutput,
) -> ToolNextAction {
    let next_max_chars = output.budget.max_chars.saturating_mul(2).min(500_000);
    let mut args = serde_json::json!({
        "path": root_display,
        "query": query,
        "max_chars": next_max_chars,
    });
    if let Some(obj) = args.as_object_mut() {
        if !inputs.include_paths.is_empty() {
            obj.insert(
                "include_paths".to_string(),
                serde_json::to_value(&inputs.include_paths).unwrap_or_default(),
            );
        }
        if !inputs.exclude_paths.is_empty() {
            obj.insert(
                "exclude_paths".to_string(),
                serde_json::to_value(&inputs.exclude_paths).unwrap_or_default(),
            );
        }
        if let Some(pattern) = inputs.file_pattern.as_deref() {
            obj.insert(
                "file_pattern".to_string(),
                serde_json::Value::String(pattern.to_string()),
            );
        }
    }

    ToolNextAction {
        tool: "context_pack".to_string(),
        args,
        reason: "Retry context_pack with a larger max_chars budget.".to_string(),
    }
}

pub(super) fn render_full(
    inputs: &ContextPackInputs,
    output: &ContextPackOutput,
    semantic_disabled_reason: Option<&str>,
) -> Vec<Content> {
    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!("context_pack: {} items", output.items.len()));
    if inputs.response_mode != ResponseMode::Minimal {
        doc.push_root_fingerprint(output.meta.root_fingerprint);
    }
    if output.items.is_empty() && inputs.response_mode != ResponseMode::Minimal {
        doc.push_note("no matches found");
    }
    if let Some(reason) = semantic_disabled_reason {
        doc.push_note("semantic: disabled (embeddings unavailable; using fuzzy-only).");
        doc.push_note(&format!("semantic_error: {reason}"));
        if output.items.is_empty() {
            doc.push_note("next: rg (semantic disabled; fallback to regex search)");
        }
    }
    for (idx, item) in output.items.iter().enumerate() {
        let mut meta_parts = Vec::new();
        meta_parts.push(format!("role={}", item.role));
        meta_parts.push(format!("score={:.3}", item.score));
        if let Some(kind) = item.chunk_type.as_deref() {
            meta_parts.push(format!("type={kind}"));
        }
        if let Some(distance) = item.distance {
            meta_parts.push(format!("distance={distance}"));
        }
        if let Some(rel) = item.relationship.as_ref().filter(|r| !r.is_empty()) {
            meta_parts.push(format!("rel={}", rel.join("->")));
        }
        if !item.imports.is_empty() {
            meta_parts.push(format!("imports={}", item.imports.len()));
        }
        doc.push_note(&format!("hit {}: {}", idx + 1, meta_parts.join(" ")));
        doc.push_ref_header(&item.file, item.start_line, item.symbol.as_deref());
        doc.push_block_smart(&item.content);
        doc.push_blank();
    }
    if output.budget.truncated {
        doc.push_note("truncated=true (increase max_chars)");
    }

    vec![Content::text(doc.finish())]
}

pub(super) fn render_default(
    inputs: &ContextPackInputs,
    output: &ContextPackOutput,
    semantic_disabled_reason: Option<&str>,
) -> Vec<Content> {
    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!("context_pack: {} items", output.items.len()));
    if inputs.response_mode != ResponseMode::Minimal {
        doc.push_root_fingerprint(output.meta.root_fingerprint);
    }
    if output.items.is_empty() && inputs.response_mode != ResponseMode::Minimal {
        doc.push_note("no matches found");
    }
    if inputs.response_mode == ResponseMode::Full {
        if let Some(reason) = semantic_disabled_reason {
            doc.push_note("semantic: disabled (embeddings unavailable; using fuzzy-only).");
            doc.push_note(&format!("semantic_error: {reason}"));
        }
    }
    for item in &output.items {
        doc.push_ref_header(&item.file, item.start_line, item.symbol.as_deref());
        doc.push_block_smart(&item.content);
        doc.push_blank();
    }
    if output.budget.truncated {
        doc.push_note("truncated=true (increase max_chars)");
    }

    vec![Content::text(doc.finish())]
}

pub(super) fn finish_result(contents: Vec<Content>, output: ContextPackOutput) -> CallToolResult {
    let mut result = CallToolResult::success(contents);
    match serde_json::to_value(&output) {
        Ok(structured) => {
            result.structured_content = Some(structured);
        }
        Err(err) => {
            return internal_error_with_meta(
                format!("Error: failed to serialize context_pack output ({err})"),
                output.meta.clone(),
            );
        }
    }
    result
}
