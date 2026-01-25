use super::super::anchor_scan::memory_best_start_line;
use super::super::candidates::is_disallowed_memory_file;
use super::super::cursors::snippet_kind_for_path;
use super::super::{
    entrypoint_candidate_score, ContextFinderService, ReadPackContext, ReadPackRequest,
    ReadPackSection, ResponseMode, REASON_ANCHOR_ENTRYPOINT, REASON_ANCHOR_FOCUS_FILE,
};
use super::section_builder::{build_section_from_file, FileSectionParams};

pub(super) fn select_entrypoint_file(
    sections: &[ReadPackSection],
    ctx: &ReadPackContext,
) -> Option<String> {
    sections.iter().find_map(|section| {
        let ReadPackSection::ProjectFacts { result } = section else {
            return None;
        };
        result
            .entry_points
            .iter()
            .filter(|rel| ctx.root.join(*rel).is_file() && !is_disallowed_memory_file(rel))
            .max_by(|a, b| {
                entrypoint_candidate_score(b)
                    .cmp(&entrypoint_candidate_score(a))
                    .then_with(|| a.cmp(b))
            })
            .cloned()
    })
}

pub(super) async fn select_focus_file(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    is_initial: bool,
) -> Option<String> {
    let focus_file = if is_initial {
        service.session.lock().await.focus_file()
    } else {
        None
    };
    focus_file.filter(|rel| ctx.root.join(rel).is_file() && !is_disallowed_memory_file(rel))
}

pub(super) async fn insert_focus_file_section(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    rel: &str,
    focus_reserved_chars: usize,
    sections: &mut Vec<ReadPackSection>,
) {
    let focus_max_lines = 140usize;
    let start_line =
        memory_best_start_line(&ctx.root, rel, focus_max_lines, snippet_kind_for_path(rel));
    let section = build_section_from_file(
        rel,
        start_line,
        FileSectionParams {
            service,
            ctx,
            request,
            response_mode,
            max_lines: focus_max_lines,
            max_chars: focus_reserved_chars,
            reason: REASON_ANCHOR_FOCUS_FILE,
            full_mode_as_file_slice: false,
        },
    )
    .await;

    let Some(section) = section else {
        return;
    };

    // Insert after project_facts and any external_memory overlays (if present).
    let mut insert_idx = sections
        .iter()
        .position(|section| matches!(section, ReadPackSection::ProjectFacts { .. }))
        .map(|idx| idx + 1)
        .unwrap_or(0);
    while insert_idx < sections.len()
        && matches!(sections[insert_idx], ReadPackSection::ExternalMemory { .. })
    {
        insert_idx += 1;
    }
    sections.insert(insert_idx, section);
}

pub(super) async fn build_entrypoint_section(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    entrypoint_file: Option<String>,
    wants_entrypoint: bool,
    entrypoint_done: &mut bool,
) -> Option<ReadPackSection> {
    if *entrypoint_done || !wants_entrypoint {
        return None;
    }
    let rel = entrypoint_file?;

    let entry_max_lines = 160usize;
    let entry_max_chars = (ctx.inner_max_chars / 8)
        .clamp(240, 3_000)
        .min(ctx.inner_max_chars);
    let start_line = memory_best_start_line(
        &ctx.root,
        &rel,
        entry_max_lines,
        snippet_kind_for_path(&rel),
    );

    let section = build_section_from_file(
        &rel,
        start_line,
        FileSectionParams {
            service,
            ctx,
            request,
            response_mode,
            max_lines: entry_max_lines,
            max_chars: entry_max_chars,
            reason: REASON_ANCHOR_ENTRYPOINT,
            full_mode_as_file_slice: true,
        },
    )
    .await;

    if section.is_some() {
        *entrypoint_done = true;
    }

    section
}

pub(super) fn insert_entrypoint_section(
    sections: &mut Vec<ReadPackSection>,
    ctx: &ReadPackContext,
    section: ReadPackSection,
) {
    let pre_entry_snippets = if ctx.max_chars <= 6_000 { 1 } else { 2 };
    let mut seen_payload = 0usize;
    let mut insert_idx = sections.len();
    for (idx, section) in sections.iter().enumerate().skip(1) {
        match section {
            ReadPackSection::Snippet { .. } | ReadPackSection::FileSlice { .. } => {
                seen_payload += 1;
                if seen_payload >= pre_entry_snippets {
                    insert_idx = (idx + 1).min(sections.len());
                    break;
                }
            }
            _ => {}
        }
    }
    sections.insert(insert_idx, section);
}
