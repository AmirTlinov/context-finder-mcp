use super::super::router::cursor_alias::{compact_cursor_alias, expand_cursor_alias};
use super::super::{compute_file_slice_result, FileSliceRequest};
use super::candidates::is_disallowed_memory_file;
use super::cursors::{snippet_kind_for_path, trimmed_non_empty_str};
use super::file_cursor::{
    decode_file_slice_cursor, ensure_cursor_file_matches_request, validate_file_slice_cursor,
};
use super::file_limits::resolve_file_slice_max_chars;
use super::{
    call_error, ReadPackContext, ReadPackNextAction, ReadPackRequest, ReadPackSection,
    ReadPackSnippet, ResponseMode,
};
use serde_json::json;

pub(super) async fn handle_file_intent(
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
    let cursor_payload = decode_file_slice_cursor(expanded_cursor.as_deref())?;
    if let Some(decoded) = cursor_payload.as_ref() {
        validate_file_slice_cursor(ctx, decoded)?;
    }

    let requested_file = trimmed_non_empty_str(request.file.as_deref()).map(ToString::to_string);
    if let (Some(decoded), Some(requested)) = (cursor_payload.as_ref(), requested_file.as_ref()) {
        ensure_cursor_file_matches_request(decoded, requested)?;
    }

    let file = requested_file.or_else(|| cursor_payload.as_ref().map(|c| c.file.clone()));
    let Some(file) = file else {
        return Err(call_error(
            "missing_field",
            "Error: file is required for intent=file",
        ));
    };

    let allow_secrets = request
        .allow_secrets
        .or_else(|| cursor_payload.as_ref().map(|c| c.allow_secrets))
        .unwrap_or(false);
    if !allow_secrets && is_disallowed_memory_file(&file) {
        return Err(call_error(
            "forbidden_file",
            "Refusing to read potential secret file via read_pack",
        ));
    }

    let max_lines = request
        .max_lines
        .or_else(|| cursor_payload.as_ref().map(|c| c.max_lines));

    let file_slice_max_chars = resolve_file_slice_max_chars(
        ctx,
        response_mode,
        cursor_payload.as_ref(),
        request.max_chars,
    );
    let mut slice = compute_file_slice_result(
        &ctx.root,
        &ctx.root_display,
        &FileSliceRequest {
            path: None,
            file: Some(file.clone()),
            start_line: request.start_line,
            max_lines,
            end_line: None,
            max_chars: Some(file_slice_max_chars),
            format: None,
            response_mode: Some(response_mode),
            allow_secrets: Some(allow_secrets),
            cursor: expanded_cursor,
        },
    )
    .map_err(|err| {
        if err.trim_start().starts_with("Invalid cursor") {
            call_error("invalid_cursor", err)
        } else {
            call_error("invalid_request", err)
        }
    })?;

    if let Some(cursor) = slice.next_cursor.take() {
        let compact = compact_cursor_alias(service, cursor).await;
        slice.next_cursor = Some(compact.clone());
        *next_cursor_out = Some(compact);
    } else {
        *next_cursor_out = None;
    }

    if response_mode == ResponseMode::Full {
        if let Some(next_cursor) = slice.next_cursor.as_deref() {
            next_actions.push(ReadPackNextAction {
                tool: "read_pack".to_string(),
                args: json!({
                    "path": ctx.root_display.clone(),
                    "intent": "file",
                    "file": file,
                    "max_lines": slice.max_lines,
                    "max_chars": ctx.max_chars,
                    "cursor": next_cursor,
                }),
                reason: "Continue reading the next page of the file slice.".to_string(),
            });
        }
    }

    if response_mode == ResponseMode::Full {
        sections.push(ReadPackSection::FileSlice { result: slice });
    } else {
        let kind = if response_mode == ResponseMode::Minimal {
            None
        } else {
            Some(snippet_kind_for_path(&file))
        };
        sections.push(ReadPackSection::Snippet {
            result: ReadPackSnippet {
                file: slice.file.clone(),
                start_line: slice.start_line,
                end_line: slice.end_line,
                content: slice.content.clone(),
                kind,
                reason: Some(super::REASON_INTENT_FILE.to_string()),
                // Cursor is already returned at the top-level (`read_pack.next_cursor`).
                // Avoid duplicating it inside the snippet: under tight budgets it can evict payload.
                next_cursor: None,
            },
        });
    }
    Ok(())
}
