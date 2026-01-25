use super::super::cursors::{ReadPackRecallCursorStoredV1, ReadPackRecallCursorV1};
use super::super::{
    call_error, encode_cursor, ContextFinderService, ReadPackContext, ResponseMode, ToolResult,
    CURSOR_VERSION, MAX_RECALL_INLINE_CURSOR_CHARS,
};
use super::input::RecallInput;
use crate::tools::cursor::cursor_fingerprint;
use crate::tools::dispatch::router::cursor_alias::compact_cursor_alias;

pub(super) async fn write_recall_cursor(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    response_mode: ResponseMode,
    input: &RecallInput,
    next_question_index: usize,
    next_cursor_out: &mut Option<String>,
) -> ToolResult<()> {
    let remaining_questions: Vec<String> = input
        .questions
        .iter()
        .skip(next_question_index)
        .cloned()
        .collect();
    if remaining_questions.is_empty() {
        return Ok(());
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
        topics: input.topics.clone(),
        include_paths: input.include_paths.clone(),
        exclude_paths: input.exclude_paths.clone(),
        file_pattern: input.file_pattern.clone(),
        prefer_code: input.prefer_code,
        include_docs: input.include_docs,
        allow_secrets: input.allow_secrets,
        next_question_index: 0,
    };

    // Try to keep cursors inline (stateless) when small; otherwise store the full continuation
    // server-side and return a tiny cursor token (agent-friendly, avoids blowing context).
    if let Ok(token) = encode_cursor(&cursor) {
        if token.len() <= MAX_RECALL_INLINE_CURSOR_CHARS {
            *next_cursor_out = Some(compact_cursor_alias(service, token).await);
            return Ok(());
        }
    }

    let stored_bytes =
        serde_json::to_vec(&cursor).map_err(|err| call_error("internal", err.to_string()))?;
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
        *next_cursor_out = Some(compact_cursor_alias(service, token).await);
    }

    Ok(())
}
