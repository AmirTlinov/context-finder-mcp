use super::super::super::router::cursor_alias::compact_cursor_alias;
use super::super::super::{compute_file_slice_result, FileSliceRequest};
use super::super::candidates::is_disallowed_memory_file;
use super::super::cursors::snippet_kind_for_path;
use super::super::{
    call_error, ContextFinderService, ReadPackContext, ReadPackSnippet, ResponseMode, ToolResult,
    REASON_NEEDLE_FILE_SLICE,
};

#[derive(Clone, Copy, Debug)]
pub(in crate::tools::dispatch::read_pack) struct SnippetFromFileParams {
    pub(in crate::tools::dispatch::read_pack) around_line: Option<usize>,
    pub(in crate::tools::dispatch::read_pack) max_lines: usize,
    pub(in crate::tools::dispatch::read_pack) max_chars: usize,
    pub(in crate::tools::dispatch::read_pack) allow_secrets: bool,
}

pub(in crate::tools::dispatch::read_pack) async fn snippet_from_file(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    file: &str,
    params: SnippetFromFileParams,
    response_mode: ResponseMode,
) -> ToolResult<ReadPackSnippet> {
    if !params.allow_secrets && is_disallowed_memory_file(file) {
        return Err(call_error(
            "forbidden_file",
            "Refusing to read potential secret file via read_pack",
        ));
    }

    let start_line = params
        .around_line
        .map(|line| line.saturating_sub(params.max_lines / 3).max(1));
    let slice = compute_file_slice_result(
        &ctx.root,
        &ctx.root_display,
        &FileSliceRequest {
            path: None,
            file: Some(file.to_string()),
            start_line,
            max_lines: Some(params.max_lines),
            end_line: None,
            max_chars: Some(params.max_chars),
            format: None,
            response_mode: Some(ResponseMode::Facts),
            allow_secrets: Some(params.allow_secrets),
            cursor: None,
        },
    )
    .map_err(|err| call_error("internal", err))?;

    let kind = if response_mode == ResponseMode::Minimal {
        None
    } else {
        Some(snippet_kind_for_path(file))
    };
    let next_cursor = if response_mode == ResponseMode::Full {
        match slice.next_cursor.clone() {
            Some(cursor) => Some(compact_cursor_alias(service, cursor).await),
            None => None,
        }
    } else {
        None
    };
    Ok(ReadPackSnippet {
        file: slice.file.clone(),
        start_line: slice.start_line,
        end_line: slice.end_line,
        content: slice.content.clone(),
        kind,
        reason: Some(REASON_NEEDLE_FILE_SLICE.to_string()),
        next_cursor,
    })
}
