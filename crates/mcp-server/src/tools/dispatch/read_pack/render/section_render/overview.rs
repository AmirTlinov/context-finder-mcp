use super::super::super::ResponseMode;
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::schemas::overview::OverviewResult;

pub(super) fn render_overview(
    doc: &mut ContextDocBuilder,
    overview: &OverviewResult,
    response_mode: ResponseMode,
) {
    doc.push_note(&format!(
        "overview: {} files={} chunks={} lines={} graph(nodes={} edges={})",
        overview.project.name,
        overview.project.files,
        overview.project.chunks,
        overview.project.lines,
        overview.graph_stats.nodes,
        overview.graph_stats.edges
    ));

    if response_mode != ResponseMode::Minimal {
        if !overview.entry_points.is_empty() {
            doc.push_note("entry_points:");
            for ep in overview.entry_points.iter().take(6) {
                doc.push_line(&format!(" - {ep}"));
            }
            if overview.entry_points.len() > 6 {
                doc.push_line(&format!(
                    " - … (showing 6 of {})",
                    overview.entry_points.len()
                ));
            }
        }
        if !overview.layers.is_empty() {
            doc.push_note("layers:");
            for layer in overview.layers.iter().take(6) {
                doc.push_line(&format!(
                    " - {} (files={}) — {}",
                    layer.name, layer.files, layer.role
                ));
            }
            if overview.layers.len() > 6 {
                doc.push_line(&format!(" - … (showing 6 of {})", overview.layers.len()));
            }
        }
        if !overview.key_types.is_empty() {
            doc.push_note("key_types:");
            for ty in overview.key_types.iter().take(6) {
                doc.push_line(&format!(
                    " - {} ({}) @ {} — coupling={}",
                    ty.name, ty.kind, ty.file, ty.coupling
                ));
            }
            if overview.key_types.len() > 6 {
                doc.push_line(&format!(" - … (showing 6 of {})", overview.key_types.len()));
            }
        }
    }

    doc.push_blank();
}
