mod budget;
mod doc_candidates;
mod section_builder;
mod selection;

use super::candidates::collect_memory_file_candidates;
use super::memory_cursor::MemoryCursorState;
use super::{
    call_error, ContextFinderService, ReadPackContext, ReadPackRequest, ReadPackSection,
    ResponseMode, ToolResult,
};
use budget::MemoryDocBudget;
use doc_candidates::{append_doc_candidates, DocCandidateParams};
use selection::{
    build_entrypoint_section, insert_entrypoint_section, insert_focus_file_section,
    select_entrypoint_file, select_focus_file,
};

pub(super) struct MemorySnippetOutcome {
    pub next_candidate_index: Option<usize>,
    pub entrypoint_done: bool,
    pub candidates_len: usize,
}

pub(super) async fn append_memory_snippets(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    cursor: MemoryCursorState,
    sections: &mut Vec<ReadPackSection>,
) -> ToolResult<MemorySnippetOutcome> {
    let entrypoint_file = select_entrypoint_file(sections, ctx);
    let focus_file = select_focus_file(service, ctx, cursor.is_initial).await;
    let wants_entrypoint = entrypoint_file.is_some() && ctx.inner_max_chars >= 1_200;
    let wants_focus_file = focus_file.is_some() && ctx.inner_max_chars >= 1_200;
    let budget = MemoryDocBudget::new(ctx, response_mode, wants_entrypoint, wants_focus_file);

    let candidates = collect_memory_file_candidates(&ctx.root);
    if cursor.start_candidate_index > candidates.len() {
        return Err(call_error("invalid_cursor", "Invalid cursor: out of range"));
    }

    if let Some(rel) = focus_file.as_deref() {
        insert_focus_file_section(
            service,
            ctx,
            request,
            response_mode,
            rel,
            budget.focus_reserved_chars,
            sections,
        )
        .await;
    }

    let next_candidate_index = append_doc_candidates(
        service,
        DocCandidateParams {
            ctx,
            request,
            response_mode,
            candidates: &candidates,
            start_candidate_index: cursor.start_candidate_index,
            docs_limit: budget.docs_limit,
            doc_max_lines: budget.doc_max_lines,
            doc_max_chars: budget.doc_max_chars,
            is_initial: cursor.is_initial,
        },
        sections,
    )
    .await;

    let mut entrypoint_done = cursor.entrypoint_done;
    let entrypoint_section = build_entrypoint_section(
        service,
        ctx,
        request,
        response_mode,
        entrypoint_file,
        wants_entrypoint,
        &mut entrypoint_done,
    )
    .await;

    if let Some(section) = entrypoint_section {
        insert_entrypoint_section(sections, ctx, section);
    }

    Ok(MemorySnippetOutcome {
        next_candidate_index,
        entrypoint_done,
        candidates_len: candidates.len(),
    })
}
