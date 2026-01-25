use super::super::super::GrepContextRequest;
use super::super::candidates::is_disallowed_memory_file;
use super::super::cursors::trimmed_non_empty_str;
use super::super::grep_cursor::{resolve_grep_pattern, resolve_grep_resume, GrepResumeCheck};
use super::super::{
    call_error, ReadPackContext, ReadPackRequest, ResponseMode, ToolResult, DEFAULT_GREP_CONTEXT,
    MAX_GREP_HUNKS, MAX_GREP_MATCHES,
};
use crate::tools::schemas::content_format::ContentFormat;
use regex::RegexBuilder;

use super::super::super::GrepContextCursorV1;

pub(super) struct GrepIntentParams {
    pub pattern: String,
    pub regex: regex::Regex,
    pub before: usize,
    pub after: usize,
    pub case_sensitive: bool,
    pub max_hunks: usize,
    pub max_matches: usize,
    pub max_chars: usize,
    pub content_max_chars: usize,
    pub resume_file: Option<String>,
    pub resume_line: usize,
    pub grep_request: GrepContextRequest,
}

pub(super) fn resolve_grep_params(
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    cursor_payload: Option<&GrepContextCursorV1>,
) -> ToolResult<GrepIntentParams> {
    let pattern = resolve_grep_pattern(request.pattern.as_deref(), cursor_payload, ctx)?;

    let case_sensitive = request
        .case_sensitive
        .or_else(|| cursor_payload.map(|c| c.case_sensitive))
        .unwrap_or(true);

    let regex = RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|err| call_error("invalid_request", format!("Invalid regex: {err}")))?;

    let before = request
        .before
        .or_else(|| cursor_payload.map(|c| c.before))
        .unwrap_or(DEFAULT_GREP_CONTEXT)
        .clamp(0, 5_000);
    let after = request
        .after
        .or_else(|| cursor_payload.map(|c| c.after))
        .unwrap_or(DEFAULT_GREP_CONTEXT)
        .clamp(0, 5_000);

    let normalized_file = trimmed_non_empty_str(request.file.as_deref())
        .map(str::to_string)
        .or_else(|| cursor_payload.and_then(|c| c.file.clone()));
    let normalized_file_pattern = trimmed_non_empty_str(request.file_pattern.as_deref())
        .map(str::to_string)
        .or_else(|| cursor_payload.and_then(|c| c.file_pattern.clone()));

    let allow_secrets = request
        .allow_secrets
        .or_else(|| cursor_payload.map(|c| c.allow_secrets))
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
    let (resume_file, resume_line) = resolve_grep_resume(cursor_payload, ctx, &resume_check)?;

    let max_chars = (ctx.inner_max_chars / 2).max(200);
    let content_max_chars = super::super::super::router::grep_context::grep_context_content_budget(
        max_chars,
        response_mode,
    );
    let max_hunks = (max_chars / 200).clamp(1, MAX_GREP_HUNKS);
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
        max_chars: Some(max_chars),
        case_sensitive: Some(case_sensitive),
        format,
        response_mode: Some(response_mode),
        allow_secrets: Some(allow_secrets),
        cursor: None,
    };

    Ok(GrepIntentParams {
        pattern,
        regex,
        before,
        after,
        case_sensitive,
        max_hunks,
        max_matches: MAX_GREP_MATCHES,
        max_chars,
        content_max_chars,
        resume_file,
        resume_line,
        grep_request,
    })
}
