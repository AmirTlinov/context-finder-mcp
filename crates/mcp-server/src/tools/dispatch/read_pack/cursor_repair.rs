use super::super::router::cursor_alias::compact_cursor_alias;
use super::candidates::{collect_memory_file_candidates, is_disallowed_memory_file};
use super::cursors::ReadPackMemoryCursorV1;
use super::{
    decode_cursor, encode_cursor, trimmed_non_empty_str, ContextFinderService, ReadPackContext,
    ReadPackIntent, ReadPackRequest, ReadPackResult, ReadPackSection, ResponseMode, CURSOR_VERSION,
};
use crate::tools::cursor::cursor_fingerprint;

pub(super) async fn repair_cursor_after_trim(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    intent: ReadPackIntent,
    response_mode: ResponseMode,
    result: &mut ReadPackResult,
) {
    match intent {
        ReadPackIntent::Memory => {
            repair_memory_cursor_after_trim(service, ctx, request, response_mode, result).await;
        }
        ReadPackIntent::Recall => {
            super::recall_trim::repair_recall_cursor_after_trim(
                service,
                ctx,
                request,
                response_mode,
                result,
            )
            .await;
        }
        _ => {}
    }
}

async fn repair_memory_cursor_after_trim(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    result: &mut ReadPackResult,
) {
    if result.next_cursor.is_some() {
        return;
    }

    let mut start_candidate_index = 0usize;
    let mut entrypoint_done_from_cursor = false;
    if let Some(cursor) = trimmed_non_empty_str(request.cursor.as_deref()) {
        if let Ok(decoded) = decode_cursor::<ReadPackMemoryCursorV1>(cursor) {
            let expected_root_hash = cursor_fingerprint(&ctx.root_display);
            let root_matches = if let Some(hash) = decoded.root_hash {
                hash == expected_root_hash
            } else {
                decoded.root.as_deref() == Some(ctx.root_display.as_str())
            };
            if decoded.v == CURSOR_VERSION
                && decoded.tool == "read_pack"
                && decoded.mode == "memory"
                && root_matches
            {
                start_candidate_index = decoded.next_candidate_index;
                entrypoint_done_from_cursor = decoded.entrypoint_done;
            }
        }
    }

    let candidates = collect_memory_file_candidates(&ctx.root);
    if candidates.is_empty() || start_candidate_index >= candidates.len() {
        return;
    }

    let mut last_idx: Option<usize> = None;
    for section in &result.sections {
        let Some(file) = read_pack_section_file(section) else {
            continue;
        };
        if let Some(idx) = candidates.iter().position(|candidate| candidate == file) {
            if idx >= start_candidate_index {
                last_idx = Some(last_idx.map_or(idx, |prev| prev.max(idx)));
            }
        }
    }
    let next_candidate_index = last_idx.map_or(start_candidate_index, |idx| idx + 1);
    if next_candidate_index >= candidates.len() {
        return;
    }

    // Avoid returning a cursor that will immediately yield an empty page.
    let has_more_payload = candidates
        .iter()
        .skip(next_candidate_index)
        .any(|rel| ctx.root.join(rel).is_file() && !is_disallowed_memory_file(rel));
    if !has_more_payload {
        return;
    }

    let entrypoint_file: Option<String> = result.sections.iter().find_map(|section| {
        let ReadPackSection::ProjectFacts { result } = section else {
            return None;
        };
        result
            .entry_points
            .iter()
            .find(|rel| ctx.root.join(*rel).is_file() && !is_disallowed_memory_file(rel))
            .cloned()
    });
    let entrypoint_in_sections = entrypoint_file.as_deref().is_some_and(|needle| {
        result
            .sections
            .iter()
            .filter_map(read_pack_section_file)
            .any(|file| file == needle)
    });
    let entrypoint_done = entrypoint_done_from_cursor || entrypoint_in_sections;

    let cursor = ReadPackMemoryCursorV1 {
        v: CURSOR_VERSION,
        tool: "read_pack".to_string(),
        mode: "memory".to_string(),
        root: Some(ctx.root_display.clone()),
        root_hash: Some(cursor_fingerprint(&ctx.root_display)),
        max_chars: Some(ctx.max_chars),
        response_mode: Some(response_mode),
        next_candidate_index,
        entrypoint_done,
    };
    if let Ok(token) = encode_cursor(&cursor) {
        result.next_cursor = Some(compact_cursor_alias(service, token).await);
    }
}

fn read_pack_section_file(section: &ReadPackSection) -> Option<&str> {
    match section {
        ReadPackSection::Snippet { result } => {
            if result.reason.as_deref() == Some(super::REASON_ANCHOR_FOCUS_FILE) {
                None
            } else {
                Some(result.file.as_str())
            }
        }
        ReadPackSection::FileSlice { result } => Some(result.file.as_str()),
        ReadPackSection::ExternalMemory { .. } => None,
        ReadPackSection::Recall { .. } => None,
        ReadPackSection::ProjectFacts { .. } => None,
        ReadPackSection::Overview { .. } => None,
        ReadPackSection::GrepContext { .. } => None,
        ReadPackSection::ContextPack { .. } => None,
        ReadPackSection::RepoOnboardingPack { .. } => None,
    }
}
