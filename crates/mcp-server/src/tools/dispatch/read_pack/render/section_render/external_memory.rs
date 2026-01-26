use super::super::super::ResponseMode;
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::schemas::read_pack::ReadPackExternalMemoryResult;

pub(super) fn render_external_memory(
    doc: &mut ContextDocBuilder,
    memory: &ReadPackExternalMemoryResult,
    response_mode: ResponseMode,
) {
    doc.push_note(&format!(
        "external_memory: source={} hits={}",
        memory.source,
        memory.hits.len()
    ));
    for hit in &memory.hits {
        let title = hit.title.as_deref().unwrap_or("");
        if title.trim().is_empty() {
            doc.push_note(&format!(
                "memory_hit: [{}] score={:.3}",
                hit.kind, hit.score
            ));
        } else {
            doc.push_note(&format!(
                "memory_hit: [{}] {} (score={:.3})",
                hit.kind, title, hit.score
            ));
        }
        if response_mode != ResponseMode::Minimal && !hit.excerpt.trim().is_empty() {
            doc.push_block_smart(&hit.excerpt);
            doc.push_blank();
        }
    }
}
