use super::super::{compute_repo_onboarding_pack_result, RepoOnboardingPackRequest};
use super::cursors::snippet_kind_for_path;
use super::onboarding_topics::{onboarding_doc_candidates, OnboardingTopic};
use super::{
    call_error, ReadPackContext, ReadPackSection, ReadPackSnippet, ResponseMode, ToolResult,
    REASON_ANCHOR_DOC,
};
use crate::tools::file_slice::compute_onboarding_doc_slice;
use std::collections::HashSet;

pub(super) struct OnboardingDocsBudget {
    pub docs_limit: usize,
    pub doc_max_lines: usize,
    pub doc_max_chars: usize,
}

impl OnboardingDocsBudget {
    pub(super) fn new(ctx: &ReadPackContext, response_mode: ResponseMode) -> Self {
        let inner = ctx.inner_max_chars.max(1);
        let mut docs_limit = if inner <= 1_400 {
            1usize
        } else if inner <= 3_000 {
            2usize
        } else if inner <= 6_000 {
            3usize
        } else {
            4usize
        };
        if response_mode == ResponseMode::Minimal {
            docs_limit = docs_limit.min(2);
        }

        // Keep per-doc slices small and deterministic so tiny budgets still return at least one
        // useful anchor.
        let doc_max_lines = if inner <= 2_000 { 80 } else { 200 };
        let doc_max_chars = (inner / (docs_limit + 2)).clamp(240, 2_000);
        Self {
            docs_limit,
            doc_max_lines,
            doc_max_chars,
        }
    }

    pub(super) fn reduce_after_command(&mut self) {
        // Noise governor: if we already surfaced an actionable command, cap anchors aggressively.
        self.docs_limit = self.docs_limit.saturating_sub(1).max(1);
    }
}

pub(super) struct OnboardingDocsParams<'a> {
    pub ctx: &'a ReadPackContext,
    pub response_mode: ResponseMode,
    pub topic: OnboardingTopic,
    pub budget: &'a OnboardingDocsBudget,
    pub sections: &'a mut Vec<ReadPackSection>,
}

pub(super) fn append_onboarding_docs(params: OnboardingDocsParams<'_>) -> usize {
    let OnboardingDocsParams {
        ctx,
        response_mode,
        topic,
        budget,
        sections,
    } = params;

    let mut seen = HashSet::new();
    let mut added = 0usize;
    for rel in onboarding_doc_candidates(topic) {
        if added >= budget.docs_limit {
            break;
        }
        if !seen.insert(rel) {
            continue;
        }
        let Ok(slice) = compute_onboarding_doc_slice(
            &ctx.root,
            rel,
            1,
            budget.doc_max_lines,
            budget.doc_max_chars,
        ) else {
            continue;
        };
        let kind = if response_mode == ResponseMode::Minimal {
            None
        } else {
            Some(snippet_kind_for_path(&slice.file))
        };
        sections.push(ReadPackSection::Snippet {
            result: ReadPackSnippet {
                file: slice.file,
                start_line: slice.start_line,
                end_line: slice.end_line,
                content: slice.content,
                kind,
                reason: Some(REASON_ANCHOR_DOC.to_string()),
                next_cursor: None,
            },
        });
        added += 1;
    }
    added
}

pub(super) async fn fallback_onboarding_docs(
    ctx: &ReadPackContext,
    response_mode: ResponseMode,
    budget: &OnboardingDocsBudget,
    sections: &mut Vec<ReadPackSection>,
) -> ToolResult<()> {
    let onboarding_request = RepoOnboardingPackRequest {
        path: Some(ctx.root_display.clone()),
        map_depth: None,
        map_limit: None,
        doc_paths: None,
        docs_limit: Some(budget.docs_limit),
        doc_max_lines: Some(budget.doc_max_lines),
        doc_max_chars: Some(budget.doc_max_chars),
        max_chars: Some(ctx.inner_max_chars),
        response_mode: None,
        auto_index: None,
        auto_index_budget_ms: None,
    };
    let mut pack =
        compute_repo_onboarding_pack_result(&ctx.root, &ctx.root_display, &onboarding_request)
            .await
            .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;
    pack.next_actions.clear();
    pack.map.next_actions = None;
    for doc in &mut pack.docs {
        doc.next_actions = None;
    }
    if response_mode == ResponseMode::Minimal {
        pack.meta.index_state = None;
        pack.map.meta = None;
        for doc in &mut pack.docs {
            doc.meta = None;
        }
    }

    for slice in pack.docs {
        let kind = if response_mode == ResponseMode::Minimal {
            None
        } else {
            Some(snippet_kind_for_path(&slice.file))
        };
        sections.push(ReadPackSection::Snippet {
            result: ReadPackSnippet {
                file: slice.file,
                start_line: slice.start_line,
                end_line: slice.end_line,
                content: slice.content,
                kind,
                reason: Some(REASON_ANCHOR_DOC.to_string()),
                next_cursor: None,
            },
        });
    }

    Ok(())
}
