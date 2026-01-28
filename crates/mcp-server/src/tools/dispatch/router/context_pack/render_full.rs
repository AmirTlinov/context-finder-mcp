use super::render_meta;
use crate::tools::context_doc::ContextDocBuilder;
use context_search::ContextPackOutput;

use super::super::super::super::{Content, ResponseMode};
use super::super::inputs::ContextPackInputs;

pub(super) fn render_full(
    inputs: &ContextPackInputs,
    output: &ContextPackOutput,
    semantic_disabled_reason: Option<&str>,
) -> Vec<Content> {
    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!("context_pack: {} items", output.items.len()));
    if inputs.format_version == 2 {
        render_meta::push_v2_envelope(&mut doc, output, semantic_disabled_reason);
        render_meta::push_next_actions_v2(&mut doc, output);
    } else {
        if inputs.response_mode != ResponseMode::Minimal {
            doc.push_root_fingerprint(output.meta.root_fingerprint);
        }
        render_meta::maybe_push_trust_micro_meta(
            &mut doc,
            inputs.response_mode,
            output,
            semantic_disabled_reason,
        );
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
    if inputs.format_version != 2 {
        render_meta::push_next_actions(&mut doc, output);
    }
    if output.budget.truncated {
        doc.push_note("truncated=true (increase max_chars)");
    }

    vec![Content::text(doc.finish())]
}
