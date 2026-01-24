use super::super::router::cursor_alias::compact_cursor_alias;
use super::super::{compute_file_slice_result, FileSliceRequest};
use super::anchor_scan::memory_best_start_line;
use super::candidates::{
    collect_memory_file_candidates, is_disallowed_memory_file, DEFAULT_MEMORY_FILE_CANDIDATES,
};
use super::cursors::snippet_kind_for_path;
use super::memory_cursor::MemoryCursorState;
use super::{
    call_error, entrypoint_candidate_score, ContextFinderService, ReadPackContext, ReadPackRequest,
    ReadPackSection, ReadPackSnippet, ResponseMode, ToolResult, REASON_ANCHOR_DOC,
    REASON_ANCHOR_ENTRYPOINT, REASON_ANCHOR_FOCUS_FILE,
};
use crate::tools::schemas::file_slice::FileSliceResult;
use std::collections::HashSet;

pub(super) struct MemorySnippetOutcome {
    pub next_candidate_index: Option<usize>,
    pub entrypoint_done: bool,
    pub candidates_len: usize,
}

#[derive(Clone, Copy)]
struct DocCandidateParams<'a> {
    ctx: &'a ReadPackContext,
    request: &'a ReadPackRequest,
    response_mode: ResponseMode,
    candidates: &'a [String],
    start_candidate_index: usize,
    docs_limit: usize,
    doc_max_lines: usize,
    doc_max_chars: usize,
    is_initial: bool,
}

#[derive(Clone, Copy)]
struct FileSectionParams<'a> {
    service: &'a ContextFinderService,
    ctx: &'a ReadPackContext,
    request: &'a ReadPackRequest,
    response_mode: ResponseMode,
    max_lines: usize,
    max_chars: usize,
    reason: &'static str,
    full_mode_as_file_slice: bool,
}

struct MemoryDocBudget {
    docs_limit: usize,
    doc_max_chars: usize,
    doc_max_lines: usize,
    focus_reserved_chars: usize,
}

impl MemoryDocBudget {
    fn new(
        ctx: &ReadPackContext,
        response_mode: ResponseMode,
        wants_entrypoint: bool,
        wants_focus_file: bool,
    ) -> Self {
        let entry_reserved_chars = if wants_entrypoint {
            (ctx.inner_max_chars / 8)
                .clamp(240, 3_000)
                .min(ctx.inner_max_chars.saturating_sub(200))
        } else {
            0
        };
        let focus_reserved_chars = if wants_focus_file {
            (ctx.inner_max_chars / 10)
                .clamp(200, 1_500)
                .min(ctx.inner_max_chars.saturating_sub(200))
        } else {
            0
        };

        let docs_budget_chars = ctx
            .inner_max_chars
            .saturating_sub(entry_reserved_chars)
            .saturating_sub(focus_reserved_chars);

        // Budgeting heuristic (agent-native):
        // - under tight budgets, prefer fewer, larger snippets (more useful than many tiny 200-char peeks)
        // - under larger budgets, expand up to a small cap to keep "memory pack" dense but broad
        //
        // The target size is intentionally coarse and deterministic: it keeps behavior stable across
        // runs and projects, while still letting callers steer results by adjusting `max_chars`.
        const MEMORY_DOC_TARGET_CHARS: usize = 800;
        let docs_limit = ((docs_budget_chars.saturating_add(MEMORY_DOC_TARGET_CHARS - 1))
            / MEMORY_DOC_TARGET_CHARS)
            .clamp(1, 6)
            .min(DEFAULT_MEMORY_FILE_CANDIDATES.len());
        let mut doc_max_chars = (docs_budget_chars / docs_limit.max(1))
            .clamp(160, 6_000)
            .min(ctx.inner_max_chars);
        if ctx.max_chars <= 1_200 {
            // Under very small budgets, prefer a smaller snippet payload so we can keep at least one
            // snippet alongside `project_facts` without popping sections during trimming.
            doc_max_chars = doc_max_chars.clamp(160, 320);
        } else if response_mode != ResponseMode::Full {
            // In low-noise modes, snippets are returned inline in the `read_pack` JSON payload.
            // JSON escaping and per-section key overhead can exceed the envelope headroom estimate
            // under small budgets, causing the final trimming pass to drop an entire snippet.
            //
            // Agent-native behavior: prefer slightly smaller snippets so the pack more often fits
            // 2+ "must-have" sections (e.g. AGENTS + README) instead of losing one to trimming.
            let (num, den) = if ctx.max_chars <= 2_500 {
                (2usize, 3usize) // tighter budgets need more headroom
            } else if ctx.max_chars <= 5_000 {
                (3usize, 4usize)
            } else {
                (4usize, 5usize)
            };
            doc_max_chars = (doc_max_chars.saturating_mul(num) / den)
                .clamp(160, 6_000)
                .min(ctx.inner_max_chars);
        }

        Self {
            docs_limit,
            doc_max_chars,
            doc_max_lines: 180,
            focus_reserved_chars,
        }
    }
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

fn select_entrypoint_file(sections: &[ReadPackSection], ctx: &ReadPackContext) -> Option<String> {
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

async fn select_focus_file(
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

async fn insert_focus_file_section(
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

async fn append_doc_candidates(
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

async fn build_entrypoint_section(
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

fn insert_entrypoint_section(
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

async fn build_section_from_file(
    rel: &str,
    start_line: usize,
    params: FileSectionParams<'_>,
) -> Option<ReadPackSection> {
    let FileSectionParams {
        service,
        ctx,
        request,
        response_mode,
        max_lines,
        max_chars,
        reason,
        full_mode_as_file_slice,
    } = params;
    let Ok(mut slice) = compute_file_slice_result(
        &ctx.root,
        &ctx.root_display,
        &FileSliceRequest {
            path: None,
            file: Some(rel.to_string()),
            start_line: Some(start_line),
            max_lines: Some(max_lines),
            end_line: None,
            max_chars: Some(max_chars),
            format: None,
            response_mode: Some(response_mode),
            allow_secrets: request.allow_secrets,
            cursor: None,
        },
    ) else {
        return None;
    };

    if response_mode == ResponseMode::Full && full_mode_as_file_slice {
        maybe_compact_slice_cursor(service, &mut slice).await;
        return Some(ReadPackSection::FileSlice { result: slice });
    }

    let kind = if response_mode == ResponseMode::Minimal {
        None
    } else {
        Some(snippet_kind_for_path(rel))
    };
    Some(ReadPackSection::Snippet {
        result: ReadPackSnippet {
            file: slice.file.clone(),
            start_line: slice.start_line,
            end_line: slice.end_line,
            content: slice.content.clone(),
            kind,
            reason: Some(reason.to_string()),
            next_cursor: None,
        },
    })
}

async fn maybe_compact_slice_cursor(service: &ContextFinderService, slice: &mut FileSliceResult) {
    if let Some(cursor) = slice.next_cursor.take() {
        slice.next_cursor = Some(compact_cursor_alias(service, cursor).await);
    }
}
