use super::super::anchor_scan::memory_best_start_line;
use super::super::cursors::snippet_kind_for_path;
use super::super::{
    ContextFinderService, ReadPackContext, ReadPackRequest, ReadPackSection, ResponseMode,
    REASON_ANCHOR_DOC,
};
use super::section_builder::{build_section_from_file, FileSectionParams};
use std::collections::HashSet;

pub(super) struct DocCandidateParams<'a> {
    pub ctx: &'a ReadPackContext,
    pub request: &'a ReadPackRequest,
    pub response_mode: ResponseMode,
    pub candidates: &'a [String],
    pub start_candidate_index: usize,
    pub docs_limit: usize,
    pub doc_max_lines: usize,
    pub doc_max_chars: usize,
    pub is_initial: bool,
}

pub(super) async fn append_doc_candidates(
    service: &ContextFinderService,
    params: DocCandidateParams<'_>,
    sections: &mut Vec<ReadPackSection>,
) -> Option<usize> {
    let DocCandidateParams {
        ctx,
        request,
        response_mode,
        candidates,
        start_candidate_index,
        docs_limit,
        doc_max_lines,
        doc_max_chars,
        is_initial,
    } = params;
    if docs_limit == 0 {
        return None;
    }

    let mut added_docs = 0usize;
    let allow_working_set_bias = is_initial;
    let seen: HashSet<String> = if allow_working_set_bias {
        let session = service.session.lock().await;
        session.seen_snippet_files_set_snapshot()
    } else {
        HashSet::new()
    };
    let mut deferred_seen: Vec<(usize, String)> = Vec::new();

    let mut next_candidate_index: Option<usize> = None;

    for (idx, rel) in candidates.iter().enumerate().skip(start_candidate_index) {
        if added_docs >= docs_limit {
            next_candidate_index = Some(idx);
            break;
        }

        let is_anchor_doc = allow_working_set_bias && idx < 2;
        if allow_working_set_bias && !is_anchor_doc && seen.contains(rel) {
            deferred_seen.push((idx, rel.clone()));
            continue;
        }

        if let Some(section) = build_doc_section(
            service,
            ctx,
            request,
            response_mode,
            rel,
            doc_max_lines,
            doc_max_chars,
        )
        .await
        {
            sections.push(section);
            added_docs += 1;
        }
    }

    // If we skipped too many already-seen docs and ran out of unseen options, backfill from
    // the deferred list (preserving candidate order).
    if added_docs < docs_limit {
        for (_, rel) in deferred_seen {
            if added_docs >= docs_limit {
                break;
            }

            if let Some(section) = build_doc_section(
                service,
                ctx,
                request,
                response_mode,
                &rel,
                doc_max_lines,
                doc_max_chars,
            )
            .await
            {
                sections.push(section);
                added_docs += 1;
            }
        }
    }

    next_candidate_index
}

async fn build_doc_section(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    rel: &str,
    doc_max_lines: usize,
    doc_max_chars: usize,
) -> Option<ReadPackSection> {
    let start_line =
        memory_best_start_line(&ctx.root, rel, doc_max_lines, snippet_kind_for_path(rel));
    build_section_from_file(
        rel,
        start_line,
        FileSectionParams {
            service,
            ctx,
            request,
            response_mode,
            max_lines: doc_max_lines,
            max_chars: doc_max_chars,
            reason: REASON_ANCHOR_DOC,
            full_mode_as_file_slice: true,
        },
    )
    .await
}
