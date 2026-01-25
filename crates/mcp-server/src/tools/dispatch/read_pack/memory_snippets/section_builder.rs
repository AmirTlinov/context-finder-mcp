use super::super::super::router::cursor_alias::compact_cursor_alias;
use super::super::super::{compute_file_slice_result, FileSliceRequest};
use super::super::cursors::snippet_kind_for_path;
use super::super::{
    ContextFinderService, ReadPackContext, ReadPackRequest, ReadPackSection, ReadPackSnippet,
    ResponseMode,
};
use crate::tools::schemas::file_slice::FileSliceResult;

pub(super) struct FileSectionParams<'a> {
    pub service: &'a ContextFinderService,
    pub ctx: &'a ReadPackContext,
    pub request: &'a ReadPackRequest,
    pub response_mode: ResponseMode,
    pub max_lines: usize,
    pub max_chars: usize,
    pub reason: &'static str,
    pub full_mode_as_file_slice: bool,
}

pub(super) async fn build_section_from_file(
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
