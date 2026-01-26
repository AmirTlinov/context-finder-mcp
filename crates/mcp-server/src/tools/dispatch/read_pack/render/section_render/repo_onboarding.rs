use super::super::super::ResponseMode;
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::schemas::repo_onboarding_pack::RepoOnboardingPackResult;

pub(super) fn render_repo_onboarding_pack(
    doc: &mut ContextDocBuilder,
    pack: &RepoOnboardingPackResult,
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
