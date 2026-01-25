use super::super::encode_cursor;
use super::super::router::cursor_alias::compact_cursor_alias;
use super::cursors::{
    normalize_optional_pattern, normalize_path_prefix_list, normalize_questions, normalize_topics,
    trimmed_non_empty_str, ReadPackRecallCursorStoredV1, ReadPackRecallCursorV1,
};
use super::recall_cursor::decode_recall_cursor;
use super::MAX_RECALL_INLINE_CURSOR_CHARS;
use super::{
    finalize_read_pack_budget, ContextFinderService, ReadPackContext, ReadPackRequest,
    ReadPackResult, ReadPackSection, ResponseMode, CURSOR_VERSION,
};
use crate::tools::cursor::cursor_fingerprint;

fn trim_string_to_chars(input: &str, max_chars: usize) -> String {
    let max_chars = max_chars.max(1);
    let mut cut_byte = input.len();
    for (seen, (idx, _)) in input.char_indices().enumerate() {
        if seen == max_chars {
            cut_byte = idx;
            break;
        }
    }
    input[..cut_byte].to_string()
}

pub(super) fn trim_recall_sections_for_budget(
    result: &mut ReadPackResult,
    max_chars: usize,
) -> std::result::Result<(), String> {
    const MIN_SNIPPET_CHARS: usize = 80;
    const MAX_ITERS: usize = 64;

    // Best-effort fine trimming: prefer dropping extra snippets (or shrinking the last snippet)
    // over dropping entire questions/sections. This significantly improves "memory UX" under
    // tight budgets: agents get *some* answer for more questions per call.
    for _ in 0..MAX_ITERS {
        finalize_read_pack_budget(result).map_err(|err| format!("{err:#}"))?;
        if result.budget.used_chars <= max_chars {
            return Ok(());
        }

        // Find the last recall section (most likely to be the one we just appended).
        let mut found = false;
        for section in result.sections.iter_mut().rev() {
            let ReadPackSection::Recall { result: recall } = section else {
                continue;
            };
            found = true;

            if recall.snippets.len() > 1 {
                recall.snippets.pop();
                break;
            }

            if let Some(snippet) = recall.snippets.last_mut() {
                let cur_len = snippet.content.chars().count();
                if cur_len > MIN_SNIPPET_CHARS {
                    let next_len = (cur_len.saturating_mul(2) / 3).max(MIN_SNIPPET_CHARS);
                    snippet.content = trim_string_to_chars(&snippet.content, next_len);
                    break;
                }
            }
        }

        if !found {
            break;
        }
    }

    Ok(())
}

pub(super) async fn repair_recall_cursor_after_trim(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    result: &mut ReadPackResult,
) {
    let (
        questions,
        topics,
        include_paths,
        exclude_paths,
        file_pattern,
        prefer_code,
        include_docs,
        allow_secrets,
    ) = if let Some(cursor) = trimmed_non_empty_str(request.cursor.as_deref()) {
        match decode_recall_cursor(service, cursor).await {
            Ok(decoded) => (
                decoded.questions,
                decoded.topics,
                decoded.include_paths,
                decoded.exclude_paths,
                decoded.file_pattern,
                decoded.prefer_code,
                decoded.include_docs,
                decoded.allow_secrets,
            ),
            Err(_) => return,
        }
    } else {
        (
            normalize_questions(request),
            normalize_topics(request),
            normalize_path_prefix_list(request.include_paths.as_ref()),
            normalize_path_prefix_list(request.exclude_paths.as_ref()),
            normalize_optional_pattern(request.file_pattern.as_deref()),
            request.prefer_code,
            request.include_docs,
            request.allow_secrets.unwrap_or(false),
        )
    };

    if questions.is_empty() {
        result.next_cursor = None;
        return;
    }

    let answered = result
        .sections
        .iter()
        .filter(|section| matches!(section, ReadPackSection::Recall { .. }))
        .count();
    if answered >= questions.len() {
        result.next_cursor = None;
        return;
    }

    let remaining_questions: Vec<String> = questions.into_iter().skip(answered).collect();
    if remaining_questions.is_empty() {
        result.next_cursor = None;
        return;
    }

    let cursor = ReadPackRecallCursorV1 {
        v: CURSOR_VERSION,
        tool: "read_pack".to_string(),
        mode: "recall".to_string(),
        root: Some(ctx.root_display.clone()),
        root_hash: Some(cursor_fingerprint(&ctx.root_display)),
        max_chars: Some(ctx.max_chars),
        response_mode: Some(response_mode),
        questions: remaining_questions,
        topics,
        include_paths,
        exclude_paths,
        file_pattern,
        prefer_code,
        include_docs,
        allow_secrets,
        next_question_index: 0,
    };

    if let Ok(token) = encode_cursor(&cursor) {
        if token.len() <= MAX_RECALL_INLINE_CURSOR_CHARS {
            result.next_cursor = Some(compact_cursor_alias(service, token).await);
            return;
        }
    }

    let stored_bytes = match serde_json::to_vec(&cursor) {
        Ok(bytes) => bytes,
        Err(_) => return,
    };
    let store_id = service.state.cursor_store_put(stored_bytes).await;
    let stored_cursor = ReadPackRecallCursorStoredV1 {
        v: CURSOR_VERSION,
        tool: "read_pack".to_string(),
        mode: "recall".to_string(),
        root: Some(ctx.root_display.clone()),
        root_hash: Some(cursor_fingerprint(&ctx.root_display)),
        max_chars: Some(ctx.max_chars),
        response_mode: Some(response_mode),
        store_id,
    };
    if let Ok(token) = encode_cursor(&stored_cursor) {
        result.next_cursor = Some(compact_cursor_alias(service, token).await);
    }
}
