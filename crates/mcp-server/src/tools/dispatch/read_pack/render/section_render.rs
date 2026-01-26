mod context_pack;
mod external_memory;
mod overview;
mod repo_onboarding;
mod snippets;

use super::super::{ReadPackSection, ResponseMode};
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
            external_memory::render_external_memory(doc, result, response_mode);
        }
        ReadPackSection::Snippet { result } => {
            snippets::render_snippet(doc, result, response_mode);
        }
        ReadPackSection::Recall { result } => {
            snippets::render_recall(doc, result, response_mode);
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
            overview::render_overview(doc, result, response_mode);
        }
        ReadPackSection::ContextPack { result } => {
            context_pack::render_context_pack(doc, result, response_mode);
        }
        ReadPackSection::RepoOnboardingPack { result } => {
            repo_onboarding::render_repo_onboarding_pack(doc, result, response_mode);
        }
    }
}
