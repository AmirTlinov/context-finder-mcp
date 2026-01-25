use super::super::cursors::trim_chars;
use super::super::{ReadPackSection, ReadPackSnippetKind, ResponseMode};
use crate::tools::context_doc::ContextDocBuilder;

pub(super) fn render_project_facts_summary(
    doc: &mut ContextDocBuilder,
    sections: &[ReadPackSection],
) {
    for section in sections {
        let ReadPackSection::ProjectFacts { result: facts } = section else {
            continue;
        };
        if !facts.ecosystems.is_empty() {
            doc.push_note(&format!("ecosystems: {}", facts.ecosystems.join(", ")));
        }
        if !facts.build_tools.is_empty() {
            doc.push_note(&format!("build_tools: {}", facts.build_tools.join(", ")));
        }
        if !facts.ci.is_empty() {
            doc.push_note(&format!("ci: {}", facts.ci.join(", ")));
        }
        if !facts.contracts.is_empty() {
            doc.push_note(&format!("contracts: {}", facts.contracts.join(", ")));
        }
        if !facts.key_dirs.is_empty() {
            doc.push_note(&format!("key_dirs: {}", facts.key_dirs.join(", ")));
        }
        if !facts.modules.is_empty() {
            doc.push_note(&format!("modules: {}", facts.modules.join(", ")));
        }
        if !facts.entry_points.is_empty() {
            doc.push_note(&format!("entry_points: {}", facts.entry_points.join(", ")));
        }
        if !facts.key_configs.is_empty() {
            doc.push_note(&format!("key_configs: {}", facts.key_configs.join(", ")));
        }
        break;
    }
}

pub(super) fn render_section(
    doc: &mut ContextDocBuilder,
    section: &ReadPackSection,
    response_mode: ResponseMode,
) {
    match section {
        ReadPackSection::ProjectFacts { .. } => {}
        ReadPackSection::ExternalMemory { result } => {
            render_external_memory(doc, result, response_mode);
        }
        ReadPackSection::Snippet { result } => {
            render_snippet(doc, result, response_mode);
        }
        ReadPackSection::Recall { result } => {
            render_recall(doc, result, response_mode);
        }
        ReadPackSection::FileSlice { result } => {
            doc.push_ref_header(&result.file, result.start_line, Some("file slice"));
            doc.push_block_smart(&result.content);
            doc.push_blank();
        }
        ReadPackSection::GrepContext { result } => {
            doc.push_note(&format!("grep: pattern={}", result.pattern));
            for hunk in &result.hunks {
                doc.push_ref_header(&hunk.file, hunk.start_line, Some("grep hunk"));
                doc.push_block_smart(&hunk.content);
                doc.push_blank();
            }
        }
        ReadPackSection::Overview { result } => {
            render_overview(doc, result, response_mode);
        }
        ReadPackSection::ContextPack { result } => {
            render_context_pack(doc, result, response_mode);
        }
        ReadPackSection::RepoOnboardingPack { result } => {
            render_repo_onboarding_pack(doc, result, response_mode);
        }
    }
}

fn render_external_memory(
    doc: &mut ContextDocBuilder,
    memory: &crate::tools::schemas::read_pack::ReadPackExternalMemoryResult,
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

fn render_snippet(
    doc: &mut ContextDocBuilder,
    snippet: &crate::tools::schemas::read_pack::ReadPackSnippet,
    response_mode: ResponseMode,
) {
    let label = snippet_label(snippet.kind);
    doc.push_ref_header(&snippet.file, snippet.start_line, label);
    if response_mode == ResponseMode::Full {
        if let Some(reason) = snippet
            .reason
            .as_deref()
            .filter(|reason| !reason.trim().is_empty())
        {
            doc.push_note(&format!("reason: {reason}"));
        }
    }
    doc.push_block_smart(&snippet.content);
    doc.push_blank();
}

fn render_recall(
    doc: &mut ContextDocBuilder,
    recall: &crate::tools::schemas::read_pack::ReadPackRecallResult,
    response_mode: ResponseMode,
) {
    doc.push_note(&format!("recall: {}", recall.question));
    for snippet in &recall.snippets {
        let label = snippet_label(snippet.kind);
        doc.push_ref_header(&snippet.file, snippet.start_line, label);
        if response_mode == ResponseMode::Full {
            if let Some(reason) = snippet
                .reason
                .as_deref()
                .filter(|reason| !reason.trim().is_empty())
            {
                doc.push_note(&format!("reason: {reason}"));
            }
        }
        doc.push_block_smart(&snippet.content);
        doc.push_blank();
    }
}

fn render_overview(
    doc: &mut ContextDocBuilder,
    overview: &crate::tools::schemas::overview::OverviewResult,
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

fn render_context_pack(
    doc: &mut ContextDocBuilder,
    pack_value: &serde_json::Value,
    response_mode: ResponseMode,
) {
    let parsed: Result<context_search::ContextPackOutput, _> =
        serde_json::from_value(pack_value.clone());
    match parsed {
        Ok(pack) => {
            let primary = pack.items.iter().filter(|i| i.role == "primary").count();
            let related = pack.items.iter().filter(|i| i.role == "related").count();
            doc.push_note(&format!(
                "context_pack: query={} items={} (primary={} related={}) truncated={} dropped_items={}",
                trim_chars(&pack.query, 80),
                pack.items.len(),
                primary,
                related,
                pack.budget.truncated,
                pack.budget.dropped_items
            ));

            if response_mode == ResponseMode::Full {
                let per_item_chars = 700usize;
                for item in pack.items.iter().take(4) {
                    doc.push_ref_header(&item.file, item.start_line, Some(item.role.as_str()));
                    if let Some(symbol) = item.symbol.as_deref() {
                        doc.push_note(&format!("symbol={} score={:.3}", symbol, item.score));
                    } else {
                        doc.push_note(&format!("score={:.3}", item.score));
                    }
                    doc.push_block_smart(&trim_chars(&item.content, per_item_chars));
                    doc.push_blank();
                }
                if pack.items.len() > 4 {
                    doc.push_note(&format!(
                        "context_pack: … (showing 4 of {} items)",
                        pack.items.len()
                    ));
                    doc.push_blank();
                }

                if !pack.next_actions.is_empty() {
                    doc.push_note("context_pack next_actions:");
                    let mut shown = 0usize;
                    for action in pack.next_actions.iter().take(3) {
                        shown += 1;
                        let args = serde_json::to_string(&action.args)
                            .unwrap_or_else(|_| "{}".to_string());
                        doc.push_line(&format!(" - {} {args}", action.tool));
                    }
                    if pack.next_actions.len() > shown {
                        doc.push_line(&format!(
                            " - … (showing {shown} of {})",
                            pack.next_actions.len()
                        ));
                    }
                    doc.push_blank();
                }
            } else {
                doc.push_blank();
            }
        }
        Err(_) => {
            doc.push_note("context_pack: (unrecognized result shape)");
            doc.push_blank();
        }
    }
}

fn render_repo_onboarding_pack(
    doc: &mut ContextDocBuilder,
    pack: &crate::tools::schemas::repo_onboarding_pack::RepoOnboardingPackResult,
    response_mode: ResponseMode,
) {
    doc.push_note(&format!(
        "repo_onboarding_pack: docs={} omitted={} truncated={}",
        pack.docs.len(),
        pack.omitted_doc_paths.len(),
        pack.budget.truncated
    ));
    if let Some(reason) = pack.docs_reason.as_ref() {
        if response_mode == ResponseMode::Full {
            doc.push_note(&format!("docs_reason={reason:?}"));
        }
    }

    if response_mode != ResponseMode::Minimal {
        doc.push_note(&format!(
            "map: dirs={} truncated={}",
            pack.map.directories.len(),
            pack.map.truncated
        ));
    }

    for doc_slice in &pack.docs {
        doc.push_ref_header(&doc_slice.file, doc_slice.start_line, Some("doc slice"));
        doc.push_block_smart(&doc_slice.content);
        doc.push_blank();
    }

    if !pack.omitted_doc_paths.is_empty() {
        doc.push_note(&format!("omitted_docs: {}", pack.omitted_doc_paths.len()));
        for path in pack.omitted_doc_paths.iter().take(10) {
            doc.push_line(&format!(" - {path}"));
        }
        if pack.omitted_doc_paths.len() > 10 {
            doc.push_line(&format!(
                " - … (showing 10 of {})",
                pack.omitted_doc_paths.len()
            ));
        }
        doc.push_blank();
    }

    if response_mode == ResponseMode::Full && !pack.next_actions.is_empty() {
        doc.push_note("repo_onboarding_pack next_actions:");
        let mut shown = 0usize;
        for action in pack.next_actions.iter().take(3) {
            shown += 1;
            let args = serde_json::to_string(&action.args).unwrap_or_else(|_| "{}".to_string());
            doc.push_line(&format!(" - {} {args}", action.tool));
        }
        if pack.next_actions.len() > shown {
            doc.push_line(&format!(
                " - … (showing {shown} of {})",
                pack.next_actions.len()
            ));
        }
        doc.push_blank();
    }
}

fn snippet_label(kind: Option<ReadPackSnippetKind>) -> Option<&'static str> {
    match kind {
        Some(ReadPackSnippetKind::Code) => Some("code"),
        Some(ReadPackSnippetKind::Doc) => Some("doc"),
        Some(ReadPackSnippetKind::Config) => Some("config"),
        None => None,
    }
}
