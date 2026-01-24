use super::super::router::cursor_alias::compact_cursor_alias;
use super::super::{decode_cursor, encode_cursor};
use super::cursors::trimmed_non_empty_str;
use super::{
    call_error, invalid_cursor_with_meta_details, ContextFinderService, ReadPackContext,
    ReadPackMemoryCursorV1, ReadPackNextAction, ReadPackRequest, ResponseMode, ToolResult,
    CURSOR_VERSION,
};
use crate::tools::cursor::cursor_fingerprint;
use context_indexer::{root_fingerprint, ToolMeta};
use serde_json::json;

#[derive(Debug, Clone, Copy)]
pub(super) struct MemoryCursorState {
    pub start_candidate_index: usize,
    pub entrypoint_done: bool,
    pub is_initial: bool,
}

pub(super) struct MemoryCursorContinuation {
    pub candidates_len: usize,
    pub next_candidate_index: Option<usize>,
    pub entrypoint_done: bool,
}

pub(super) struct MemoryCursorOutput<'a> {
    pub next_actions: &'a mut Vec<ReadPackNextAction>,
    pub next_cursor_out: &'a mut Option<String>,
}

pub(super) fn decode_memory_cursor(
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
) -> ToolResult<MemoryCursorState> {
    let cursor = trimmed_non_empty_str(request.cursor.as_deref());
    if cursor.is_none() {
        return Ok(MemoryCursorState {
            start_candidate_index: 0,
            entrypoint_done: false,
            is_initial: true,
        });
    }

    let overrides = request.file.is_some()
        || request.pattern.is_some()
        || request.query.is_some()
        || request.ask.is_some()
        || request.questions.is_some()
        || request.topics.is_some()
        || request.file_pattern.is_some()
        || request.include_paths.is_some()
        || request.exclude_paths.is_some()
        || request.before.is_some()
        || request.after.is_some()
        || request.case_sensitive.is_some()
        || request.start_line.is_some()
        || request.prefer_code.is_some()
        || request.include_docs.is_some();
    if overrides {
        return Err(call_error(
            "invalid_cursor",
            "Cursor continuation does not allow overriding memory parameters",
        ));
    }

    let decoded: ReadPackMemoryCursorV1 = decode_cursor(cursor.unwrap())
        .map_err(|err| call_error("invalid_cursor", format!("Invalid cursor: {err}")))?;
    if decoded.v != CURSOR_VERSION || decoded.tool != "read_pack" || decoded.mode != "memory" {
        return Err(call_error("invalid_cursor", "Invalid cursor: wrong tool"));
    }

    let expected_root_hash = cursor_fingerprint(&ctx.root_display);
    let expected_root_fingerprint = root_fingerprint(&ctx.root_display);
    if let Some(hash) = decoded.root_hash {
        if hash != expected_root_hash {
            return Err(invalid_cursor_with_meta_details(
                "Invalid cursor: different root",
                ToolMeta {
                    root_fingerprint: Some(expected_root_fingerprint),
                    ..ToolMeta::default()
                },
                json!({
                    "expected_root_fingerprint": expected_root_fingerprint,
                    "cursor_root_fingerprint": Some(hash),
                }),
            ));
        }
    } else if decoded.root.as_deref() != Some(ctx.root_display.as_str()) {
        let cursor_root_fingerprint = decoded.root.as_deref().map(root_fingerprint);
        return Err(invalid_cursor_with_meta_details(
            "Invalid cursor: different root",
            ToolMeta {
                root_fingerprint: Some(expected_root_fingerprint),
                ..ToolMeta::default()
            },
            json!({
                "expected_root_fingerprint": expected_root_fingerprint,
                "cursor_root_fingerprint": cursor_root_fingerprint,
            }),
        ));
    }

    Ok(MemoryCursorState {
        start_candidate_index: decoded.next_candidate_index,
        entrypoint_done: decoded.entrypoint_done,
        is_initial: false,
    })
}

pub(super) async fn write_memory_cursor(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    response_mode: ResponseMode,
    continuation: MemoryCursorContinuation,
    output: MemoryCursorOutput<'_>,
) -> ToolResult<()> {
    let MemoryCursorContinuation {
        candidates_len,
        next_candidate_index,
        entrypoint_done,
    } = continuation;
    let MemoryCursorOutput {
        next_actions,
        next_cursor_out,
    } = output;

    if let Some(next_index) = next_candidate_index {
        if next_index < candidates_len {
            let cursor = ReadPackMemoryCursorV1 {
                v: CURSOR_VERSION,
                tool: "read_pack".to_string(),
                mode: "memory".to_string(),
                root: Some(ctx.root_display.clone()),
                root_hash: Some(cursor_fingerprint(&ctx.root_display)),
                max_chars: Some(ctx.max_chars),
                response_mode: Some(response_mode),
                next_candidate_index: next_index,
                entrypoint_done,
            };
            if let Ok(token) = encode_cursor(&cursor) {
                let compact = compact_cursor_alias(service, token).await;
                *next_cursor_out = Some(compact);
            } else {
                *next_cursor_out = None;
            }

            if response_mode == ResponseMode::Full {
                if let Some(next_cursor) = next_cursor_out.as_deref() {
                    next_actions.push(ReadPackNextAction {
                        tool: "read_pack".to_string(),
                        args: json!({
                            "path": ctx.root_display.clone(),
                            "max_chars": ctx.max_chars,
                            "cursor": next_cursor,
                        }),
                        reason: "Continue the memory-pack (next page of high-signal snippets)."
                            .to_string(),
                    });
                }
            }
        }
    }

    Ok(())
}
