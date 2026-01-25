use super::super::router::cursor_alias::{compact_cursor_alias, expand_cursor_alias};
use super::super::{compute_grep_context_result, GrepContextComputeOptions, GrepContextRequest};
use super::candidates::is_disallowed_memory_file;
use super::cursors::{snippet_kind_for_path, trimmed_non_empty_str};
use super::grep_cursor::{
    decode_grep_cursor, resolve_grep_pattern, resolve_grep_resume, GrepResumeCheck,
};
use super::{
    call_error, ReadPackContext, ReadPackNextAction, ReadPackRequest, ReadPackSection,
    ReadPackSnippet, ResponseMode, DEFAULT_GREP_CONTEXT, MAX_GREP_HUNKS, MAX_GREP_MATCHES,
};
use crate::tools::schemas::content_format::ContentFormat;
use regex::RegexBuilder;
use serde_json::json;

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
    let pattern = resolve_grep_pattern(request.pattern.as_deref(), cursor_payload.as_ref(), ctx)?;

    let case_sensitive = request
        .case_sensitive
        .or_else(|| cursor_payload.as_ref().map(|c| c.case_sensitive))
        .unwrap_or(true);
    let regex = RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|err| call_error("invalid_request", format!("Invalid regex: {err}")))?;

    let before = request
        .before
        .or_else(|| cursor_payload.as_ref().map(|c| c.before))
        .unwrap_or(DEFAULT_GREP_CONTEXT)
        .clamp(0, 5_000);
    let after = request
        .after
        .or_else(|| cursor_payload.as_ref().map(|c| c.after))
        .unwrap_or(DEFAULT_GREP_CONTEXT)
        .clamp(0, 5_000);

    let normalized_file = trimmed_non_empty_str(request.file.as_deref())
        .map(str::to_string)
        .or_else(|| cursor_payload.as_ref().and_then(|c| c.file.clone()));
    let normalized_file_pattern = trimmed_non_empty_str(request.file_pattern.as_deref())
        .map(str::to_string)
        .or_else(|| cursor_payload.as_ref().and_then(|c| c.file_pattern.clone()));

    let allow_secrets = request
        .allow_secrets
        .or_else(|| cursor_payload.as_ref().map(|c| c.allow_secrets))
        .unwrap_or(false);
    if !allow_secrets {
        if let Some(file) = normalized_file.as_deref() {
            if is_disallowed_memory_file(file) {
                return Err(call_error(
                    "forbidden_file",
                    "Refusing to search potential secret file via read_pack",
                ));
            }
        }
    }

    let resume_check = GrepResumeCheck {
        pattern: pattern.as_str(),
        file: normalized_file.as_ref(),
        file_pattern: normalized_file_pattern.as_ref(),
        case_sensitive,
        before,
        after,
        allow_secrets,
    };
    let (resume_file, resume_line) =
        resolve_grep_resume(cursor_payload.as_ref(), ctx, &resume_check)?;

    let grep_max_chars = (ctx.inner_max_chars / 2).max(200);
    let grep_content_max_chars = super::super::router::grep_context::grep_context_content_budget(
        grep_max_chars,
        response_mode,
    );
    let max_hunks = (grep_max_chars / 200).clamp(1, MAX_GREP_HUNKS);
    let format = match response_mode {
        ResponseMode::Full => None,
        ResponseMode::Facts | ResponseMode::Minimal => Some(ContentFormat::Plain),
    };
    let grep_request = GrepContextRequest {
        path: None,
        pattern: Some(pattern.clone()),
        literal: Some(false),
        file: normalized_file,
        file_pattern: normalized_file_pattern,
        context: None,
        before: Some(before),
        after: Some(after),
        max_matches: Some(MAX_GREP_MATCHES),
        max_hunks: Some(max_hunks),
        max_chars: Some(grep_max_chars),
        case_sensitive: Some(case_sensitive),
        format,
        response_mode: Some(response_mode),
        allow_secrets: Some(allow_secrets),
        cursor: None,
    };

    let mut result = compute_grep_context_result(
        &ctx.root,
        &ctx.root_display,
        &grep_request,
        &regex,
        GrepContextComputeOptions {
            case_sensitive,
            before,
            after,
            max_matches: MAX_GREP_MATCHES,
            max_hunks,
            max_chars: grep_max_chars,
            content_max_chars: grep_content_max_chars,
            resume_file: resume_file.as_deref(),
            resume_line,
        },
    )
    .await
    .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;

    if let Some(cursor) = result.next_cursor.take() {
        let compact = compact_cursor_alias(service, cursor).await;
        result.next_cursor = Some(compact.clone());
        *next_cursor_out = Some(compact);
    } else {
        *next_cursor_out = None;
    }

    if response_mode == ResponseMode::Full {
        if let Some(next_cursor) = result.next_cursor.as_deref() {
            let GrepContextRequest {
                file, file_pattern, ..
            } = grep_request;
            next_actions.push(ReadPackNextAction {
                tool: "read_pack".to_string(),
                args: json!({
                    "path": ctx.root_display.clone(),
                    "intent": "grep",
                    "pattern": pattern,
                    "file": file,
                    "file_pattern": file_pattern,
                    "before": before,
                    "after": after,
                    "case_sensitive": case_sensitive,
                    "max_chars": ctx.max_chars,
                    "cursor": next_cursor,
                }),
                reason: "Continue rg pagination (next page of hunks).".to_string(),
            });
        }
    }

    if response_mode == ResponseMode::Full {
        sections.push(ReadPackSection::GrepContext { result });
    } else {
        for hunk in result.hunks.iter().take(3) {
            let kind = if response_mode == ResponseMode::Minimal {
                None
            } else {
                Some(snippet_kind_for_path(&hunk.file))
            };
            sections.push(ReadPackSection::Snippet {
                result: ReadPackSnippet {
                    file: hunk.file.clone(),
                    start_line: hunk.start_line,
                    end_line: hunk.end_line,
                    content: hunk.content.clone(),
                    kind,
                    reason: Some(super::REASON_NEEDLE_GREP_HUNK.to_string()),
                    next_cursor: None,
                },
            });
        }
    }
    Ok(())
}
