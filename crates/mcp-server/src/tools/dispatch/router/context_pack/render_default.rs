use super::render_meta;
use crate::tools::context_doc::ContextDocBuilder;
use context_search::ContextPackOutput;

use super::super::super::super::{Content, ResponseMode};
use super::super::inputs::ContextPackInputs;

pub(super) fn render_default(
    inputs: &ContextPackInputs,
    output: &ContextPackOutput,
    semantic_disabled_reason: Option<&str>,
) -> Vec<Content> {
    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!("context_pack: {} items", output.items.len()));
    if inputs.format_version == 2 {
        // v2: make trust/provenance/next explicit and always present (compact).
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
    if inputs.format_version != 2 && inputs.response_mode != ResponseMode::Minimal {
        render_meta::push_next_actions(&mut doc, output);
    }
    if output.budget.truncated {
        doc.push_note("truncated=true (increase max_chars)");
    }

    vec![Content::text(doc.finish())]
}
