use super::memory_cursor::{
    decode_memory_cursor, write_memory_cursor, MemoryCursorContinuation, MemoryCursorOutput,
};
use super::memory_overview::{insert_external_memory_overlays, maybe_add_overview};
use super::memory_snippets::append_memory_snippets;
use super::{
    ContextFinderService, ReadPackContext, ReadPackNextAction, ReadPackRequest, ReadPackSection,
    ResponseMode, ToolResult,
};

pub(super) async fn handle_memory_intent(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    sections: &mut Vec<ReadPackSection>,
    next_actions: &mut Vec<ReadPackNextAction>,
    next_cursor_out: &mut Option<String>,
) -> ToolResult<()> {
    maybe_add_overview(service, ctx, response_mode, sections).await;
    insert_external_memory_overlays(ctx, request, response_mode, sections).await;

    let cursor_state = decode_memory_cursor(ctx, request)?;
    let outcome =
        append_memory_snippets(service, ctx, request, response_mode, cursor_state, sections)
            .await?;

    let continuation = MemoryCursorContinuation {
        candidates_len: outcome.candidates_len,
        next_candidate_index: outcome.next_candidate_index,
        entrypoint_done: outcome.entrypoint_done,
    };
    let output = MemoryCursorOutput {
        next_actions,
        next_cursor_out,
    };

    write_memory_cursor(service, ctx, response_mode, continuation, output).await?;

    Ok(())
}
