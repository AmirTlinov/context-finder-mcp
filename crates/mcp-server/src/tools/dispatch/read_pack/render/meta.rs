use super::super::ReadPackSection;
use context_indexer::ToolMeta;

pub(in crate::tools::dispatch::read_pack) fn apply_meta_to_sections(
    sections: &mut [ReadPackSection],
) {
    for section in sections {
        match section {
            ReadPackSection::ProjectFacts { .. } => {}
            ReadPackSection::ExternalMemory { .. } => {}
            ReadPackSection::Snippet { .. } => {}
            ReadPackSection::Recall { .. } => {}
            ReadPackSection::Overview { result } => {
                result.meta = ToolMeta::default();
            }
            ReadPackSection::FileSlice { result } => {
                result.meta = None;
            }
            ReadPackSection::GrepContext { result } => {
                result.meta = None;
            }
            ReadPackSection::RepoOnboardingPack { result } => {
                result.meta = ToolMeta::default();
            }
            ReadPackSection::ContextPack { .. } => {}
        }
    }
}
