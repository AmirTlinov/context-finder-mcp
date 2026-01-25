mod params;
mod render;

use params::resolve_grep_params;
use render::{finalize_grep_result, GrepFinalizeOutput};

use super::super::router::cursor_alias::expand_cursor_alias;
use super::super::{compute_grep_context_result, GrepContextComputeOptions};
use super::cursors::trimmed_non_empty_str;
use super::grep_cursor::decode_grep_cursor;
use super::{
    call_error, ReadPackContext, ReadPackNextAction, ReadPackRequest, ReadPackSection, ResponseMode,
};

pub(super) async fn handle_grep_intent(
    service: &super::ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    sections: &mut Vec<ReadPackSection>,
    next_actions: &mut Vec<ReadPackNextAction>,
    next_cursor_out: &mut Option<String>,
) -> super::ToolResult<()> {
    let expanded_cursor = match trimmed_non_empty_str(request.cursor.as_deref()) {
        Some(cursor) => Some(
            expand_cursor_alias(service, cursor)
                .await
                .map_err(|message| call_error("invalid_cursor", message))?,
        ),
        None => None,
    };

    let cursor_payload = decode_grep_cursor(expanded_cursor.as_deref())?;
    let params = resolve_grep_params(ctx, request, response_mode, cursor_payload.as_ref())?;

    let result = compute_grep_context_result(
        &ctx.root,
        &ctx.root_display,
        &params.grep_request,
        &params.regex,
        GrepContextComputeOptions {
            case_sensitive: params.case_sensitive,
            before: params.before,
            after: params.after,
            max_matches: params.max_matches,
            max_hunks: params.max_hunks,
            max_chars: params.max_chars,
            content_max_chars: params.content_max_chars,
            resume_file: params.resume_file.as_deref(),
            resume_line: params.resume_line,
        },
    )
    .await
    .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;

    finalize_grep_result(
        service,
        ctx,
        response_mode,
        params,
        result,
        GrepFinalizeOutput {
            sections,
            next_actions,
            next_cursor_out,
        },
    )
    .await?;

    Ok(())
}
