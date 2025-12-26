use super::super::{
    compute_file_slice_result, compute_grep_context_result, compute_repo_onboarding_pack_result,
    decode_cursor, finalize_read_pack_budget, CallToolResult, Content, ContextFinderService,
    ContextPackRequest, FileSliceCursorV1, FileSliceRequest, GrepContextComputeOptions,
    GrepContextCursorV1, GrepContextRequest, McpError, Parameters, ReadPackBudget, ReadPackIntent,
    ReadPackNextAction, ReadPackRequest, ReadPackResult, ReadPackSection, ReadPackTruncation,
    RepoOnboardingPackRequest, CURSOR_VERSION,
};
use context_indexer::ToolMeta;
use regex::RegexBuilder;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

const VERSION: u32 = 1;
const DEFAULT_MAX_CHARS: usize = 20_000;
const MIN_MAX_CHARS: usize = 1_000;
const MAX_MAX_CHARS: usize = 500_000;
const DEFAULT_GREP_CONTEXT: usize = 20;
const MAX_GREP_MATCHES: usize = 10_000;
const MAX_GREP_HUNKS: usize = 200;
const DEFAULT_TIMEOUT_MS: u64 = 55_000;
const MAX_TIMEOUT_MS: u64 = 300_000;

type ToolResult<T> = std::result::Result<T, CallToolResult>;

#[derive(Debug, Deserialize)]
struct CursorHeader {
    v: u32,
    tool: String,
}

fn call_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.into())])
}

fn trimmed_non_empty_str(input: Option<&str>) -> Option<&str> {
    input.map(str::trim).filter(|value| !value.is_empty())
}

struct ReadPackContext {
    root: PathBuf,
    root_display: String,
    max_chars: usize,
    inner_max_chars: usize,
}

fn build_context(
    request: &ReadPackRequest,
    root: PathBuf,
    root_display: String,
) -> ToolResult<ReadPackContext> {
    let max_chars = request
        .max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);
    // Inner tool budgets leave headroom for JSON overhead (especially `\\n` escaping).
    let inner_max_chars = (max_chars.saturating_mul(3) / 5).max(1_000).min(max_chars);

    Ok(ReadPackContext {
        root,
        root_display,
        max_chars,
        inner_max_chars,
    })
}

fn resolve_intent(request: &ReadPackRequest) -> ToolResult<ReadPackIntent> {
    let mut intent = request.intent.unwrap_or(ReadPackIntent::Auto);
    if !matches!(intent, ReadPackIntent::Auto) {
        return Ok(intent);
    }

    if let Some(cursor) = trimmed_non_empty_str(request.cursor.as_deref()) {
        let header: CursorHeader =
            decode_cursor(cursor).map_err(|err| call_error(format!("Invalid cursor: {err}")))?;
        if header.v != CURSOR_VERSION {
            return Err(call_error("Invalid cursor: wrong version"));
        }
        intent = match header.tool.as_str() {
            "file_slice" => ReadPackIntent::File,
            "grep_context" => ReadPackIntent::Grep,
            _ => return Err(call_error("Invalid cursor: unsupported tool for read_pack")),
        };
        return Ok(intent);
    }

    if trimmed_non_empty_str(request.query.as_deref()).is_some() {
        return Ok(ReadPackIntent::Query);
    }
    if trimmed_non_empty_str(request.pattern.as_deref()).is_some() {
        return Ok(ReadPackIntent::Grep);
    }
    if trimmed_non_empty_str(request.file.as_deref()).is_some() {
        return Ok(ReadPackIntent::File);
    }

    Ok(ReadPackIntent::Onboarding)
}

fn intent_label(intent: ReadPackIntent) -> &'static str {
    match intent {
        ReadPackIntent::Auto => "auto",
        ReadPackIntent::File => "file",
        ReadPackIntent::Grep => "grep",
        ReadPackIntent::Query => "query",
        ReadPackIntent::Onboarding => "onboarding",
    }
}

fn finalize_and_trim(mut result: ReadPackResult, max_chars: usize) -> ToolResult<ReadPackResult> {
    finalize_read_pack_budget(&mut result).map_err(|err| call_error(format!("Error: {err:#}")))?;

    while result.budget.used_chars > max_chars && result.next_actions.len() > 1 {
        result.next_actions.pop();
        result.budget.truncated = true;
        result.budget.truncation = Some(ReadPackTruncation::MaxChars);
        let _ = finalize_read_pack_budget(&mut result);
    }
    while result.budget.used_chars > max_chars && result.sections.len() > 1 {
        result.sections.pop();
        result.budget.truncated = true;
        result.budget.truncation = Some(ReadPackTruncation::MaxChars);
        let _ = finalize_read_pack_budget(&mut result);
    }
    if result.budget.used_chars > max_chars {
        result.budget.truncated = true;
        result.budget.truncation = Some(ReadPackTruncation::MaxChars);
        let _ = finalize_read_pack_budget(&mut result);
    }

    Ok(result)
}

fn ensure_retry_action(
    result: &mut ReadPackResult,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    intent: ReadPackIntent,
) {
    if !result.budget.truncated || !result.next_actions.is_empty() {
        return;
    }

    let suggested_max_chars = ctx
        .max_chars
        .saturating_mul(2)
        .clamp(DEFAULT_MAX_CHARS, MAX_MAX_CHARS);

    let mut args = serde_json::Map::new();
    args.insert(
        "path".to_string(),
        serde_json::Value::String(ctx.root_display.clone()),
    );
    args.insert(
        "intent".to_string(),
        serde_json::Value::String(intent_label(intent).to_string()),
    );
    args.insert(
        "max_chars".to_string(),
        serde_json::Value::Number(suggested_max_chars.into()),
    );

    if let Some(cursor) = trimmed_non_empty_str(request.cursor.as_deref()) {
        args.insert(
            "cursor".to_string(),
            serde_json::Value::String(cursor.to_string()),
        );
    }

    match intent {
        ReadPackIntent::File => {
            if let Some(file) = trimmed_non_empty_str(request.file.as_deref()) {
                args.insert(
                    "file".to_string(),
                    serde_json::Value::String(file.to_string()),
                );
            }
            if let Some(start_line) = request.start_line {
                args.insert(
                    "start_line".to_string(),
                    serde_json::Value::Number(start_line.into()),
                );
            }
            if let Some(max_lines) = request.max_lines {
                args.insert(
                    "max_lines".to_string(),
                    serde_json::Value::Number(max_lines.into()),
                );
            }
        }
        ReadPackIntent::Grep => {
            if let Some(pattern) = trimmed_non_empty_str(request.pattern.as_deref()) {
                args.insert(
                    "pattern".to_string(),
                    serde_json::Value::String(pattern.to_string()),
                );
            }
            if let Some(file_pattern) = trimmed_non_empty_str(request.file_pattern.as_deref()) {
                args.insert(
                    "file_pattern".to_string(),
                    serde_json::Value::String(file_pattern.to_string()),
                );
            }
            if let Some(before) = request.before {
                args.insert(
                    "before".to_string(),
                    serde_json::Value::Number(before.into()),
                );
            }
            if let Some(after) = request.after {
                args.insert("after".to_string(), serde_json::Value::Number(after.into()));
            }
            if let Some(case_sensitive) = request.case_sensitive {
                args.insert(
                    "case_sensitive".to_string(),
                    serde_json::Value::Bool(case_sensitive),
                );
            }
        }
        ReadPackIntent::Query => {
            if let Some(query) = trimmed_non_empty_str(request.query.as_deref()) {
                args.insert(
                    "query".to_string(),
                    serde_json::Value::String(query.to_string()),
                );
            }
            if let Some(prefer_code) = request.prefer_code {
                args.insert(
                    "prefer_code".to_string(),
                    serde_json::Value::Bool(prefer_code),
                );
            }
            if let Some(include_docs) = request.include_docs {
                args.insert(
                    "include_docs".to_string(),
                    serde_json::Value::Bool(include_docs),
                );
            }
        }
        ReadPackIntent::Onboarding | ReadPackIntent::Auto => {}
    }

    result.next_actions.push(ReadPackNextAction {
        tool: "read_pack".to_string(),
        args: serde_json::Value::Object(args),
        reason: "Increase max_chars to get a fuller read_pack payload.".to_string(),
    });
    let _ = finalize_read_pack_budget(result);
}

fn apply_meta_to_sections(meta: &ToolMeta, sections: &mut [ReadPackSection]) {
    for section in sections {
        match section {
            ReadPackSection::FileSlice { result } => {
                result.meta = Some(meta.clone());
            }
            ReadPackSection::GrepContext { result } => {
                result.meta = Some(meta.clone());
            }
            ReadPackSection::RepoOnboardingPack { result } => {
                result.meta = Some(meta.clone());
            }
            ReadPackSection::ContextPack { .. } => {}
        }
    }
}

fn decode_file_slice_cursor(cursor: Option<&str>) -> ToolResult<Option<FileSliceCursorV1>> {
    let Some(cursor) = trimmed_non_empty_str(cursor) else {
        return Ok(None);
    };

    let decoded: FileSliceCursorV1 =
        decode_cursor(cursor).map_err(|err| call_error(format!("Invalid cursor: {err}")))?;
    Ok(Some(decoded))
}

fn handle_file_intent(
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    sections: &mut Vec<ReadPackSection>,
    next_actions: &mut Vec<ReadPackNextAction>,
) -> ToolResult<()> {
    let requested_file = trimmed_non_empty_str(request.file.as_deref()).map(ToString::to_string);
    let need_cursor_defaults = requested_file.is_none() || request.max_lines.is_none();
    let cursor_payload = if need_cursor_defaults {
        match decode_file_slice_cursor(request.cursor.as_deref())? {
            Some(decoded) => {
                if decoded.v != CURSOR_VERSION || decoded.tool != "file_slice" {
                    return Err(call_error("Invalid cursor: wrong tool"));
                }
                if decoded.root != ctx.root_display {
                    return Err(call_error("Invalid cursor: different root"));
                }
                Some(decoded)
            }
            None => None,
        }
    } else {
        None
    };

    let file = requested_file.or_else(|| cursor_payload.as_ref().map(|c| c.file.clone()));
    let Some(file) = file else {
        return Err(call_error("Error: file is required for intent=file"));
    };

    let max_lines = request
        .max_lines
        .or_else(|| cursor_payload.as_ref().map(|c| c.max_lines));
    let slice = compute_file_slice_result(
        &ctx.root,
        &ctx.root_display,
        &FileSliceRequest {
            path: None,
            file: file.clone(),
            start_line: request.start_line,
            max_lines,
            max_chars: Some(ctx.inner_max_chars),
            cursor: request.cursor.clone(),
        },
    )
    .map_err(call_error)?;

    if let Some(next_cursor) = slice.next_cursor.as_deref() {
        next_actions.push(ReadPackNextAction {
            tool: "read_pack".to_string(),
            args: serde_json::json!({
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

    sections.push(ReadPackSection::FileSlice { result: slice });
    Ok(())
}

fn decode_grep_cursor(cursor: Option<&str>) -> ToolResult<Option<GrepContextCursorV1>> {
    let Some(cursor) = trimmed_non_empty_str(cursor) else {
        return Ok(None);
    };

    let decoded: GrepContextCursorV1 =
        decode_cursor(cursor).map_err(|err| call_error(format!("Invalid cursor: {err}")))?;
    Ok(Some(decoded))
}

fn validate_grep_cursor_tool_root(
    decoded: &GrepContextCursorV1,
    root_display: &str,
) -> ToolResult<()> {
    if decoded.v != CURSOR_VERSION || decoded.tool != "grep_context" {
        return Err(call_error("Invalid cursor: wrong tool"));
    }
    if decoded.root != root_display {
        return Err(call_error("Invalid cursor: different root"));
    }
    Ok(())
}

fn resolve_grep_pattern(
    request_pattern: Option<&str>,
    cursor_payload: Option<&GrepContextCursorV1>,
    root_display: &str,
) -> ToolResult<String> {
    if let Some(pattern) = trimmed_non_empty_str(request_pattern) {
        return Ok(pattern.to_string());
    }

    if let Some(decoded) = cursor_payload {
        validate_grep_cursor_tool_root(decoded, root_display)?;
        return Ok(decoded.pattern.clone());
    }

    Err(call_error("Error: pattern is required for intent=grep"))
}

struct GrepResumeCheck<'a> {
    pattern: &'a str,
    file: Option<&'a String>,
    file_pattern: Option<&'a String>,
    case_sensitive: bool,
    before: usize,
    after: usize,
}

fn resolve_grep_resume(
    cursor_payload: Option<&GrepContextCursorV1>,
    root_display: &str,
    check: &GrepResumeCheck<'_>,
) -> ToolResult<(Option<String>, usize)> {
    let Some(decoded) = cursor_payload else {
        return Ok((None, 1));
    };
    validate_grep_cursor_tool_root(decoded, root_display)?;

    if decoded.pattern != check.pattern {
        return Err(call_error("Invalid cursor: different pattern"));
    }
    if decoded.file.as_ref() != check.file {
        return Err(call_error("Invalid cursor: different file"));
    }
    if decoded.file_pattern.as_ref() != check.file_pattern {
        return Err(call_error("Invalid cursor: different file_pattern"));
    }
    if decoded.case_sensitive != check.case_sensitive
        || decoded.before != check.before
        || decoded.after != check.after
    {
        return Err(call_error("Invalid cursor: different search options"));
    }

    Ok((
        Some(decoded.resume_file.clone()),
        decoded.resume_line.max(1),
    ))
}

async fn handle_grep_intent(
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    sections: &mut Vec<ReadPackSection>,
    next_actions: &mut Vec<ReadPackNextAction>,
) -> ToolResult<()> {
    let cursor_payload = decode_grep_cursor(request.cursor.as_deref())?;
    let pattern = resolve_grep_pattern(
        request.pattern.as_deref(),
        cursor_payload.as_ref(),
        &ctx.root_display,
    )?;

    let case_sensitive = request
        .case_sensitive
        .or_else(|| cursor_payload.as_ref().map(|c| c.case_sensitive))
        .unwrap_or(true);
    let regex = RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|err| call_error(format!("Invalid regex: {err}")))?;

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

    let resume_check = GrepResumeCheck {
        pattern: pattern.as_str(),
        file: normalized_file.as_ref(),
        file_pattern: normalized_file_pattern.as_ref(),
        case_sensitive,
        before,
        after,
    };
    let (resume_file, resume_line) =
        resolve_grep_resume(cursor_payload.as_ref(), &ctx.root_display, &resume_check)?;

    let grep_max_chars = (ctx.inner_max_chars / 2).max(200);
    let max_hunks = (grep_max_chars / 200).clamp(1, MAX_GREP_HUNKS);
    let grep_request = GrepContextRequest {
        path: None,
        pattern: pattern.clone(),
        file: normalized_file,
        file_pattern: normalized_file_pattern,
        context: None,
        before: Some(before),
        after: Some(after),
        max_matches: Some(MAX_GREP_MATCHES),
        max_hunks: Some(max_hunks),
        max_chars: Some(grep_max_chars),
        case_sensitive: Some(case_sensitive),
        cursor: None,
    };

    let result = compute_grep_context_result(
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
            resume_file: resume_file.as_deref(),
            resume_line,
        },
    )
    .await
    .map_err(|err| call_error(format!("Error: {err:#}")))?;

    if let Some(next_cursor) = result.next_cursor.as_deref() {
        let GrepContextRequest {
            file, file_pattern, ..
        } = grep_request;
        next_actions.push(ReadPackNextAction {
            tool: "read_pack".to_string(),
            args: serde_json::json!({
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
            reason: "Continue grep_context pagination (next page of hunks).".to_string(),
        });
    }

    sections.push(ReadPackSection::GrepContext { result });
    Ok(())
}

async fn handle_query_intent(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    sections: &mut Vec<ReadPackSection>,
) -> ToolResult<()> {
    let query = trimmed_non_empty_str(request.query.as_deref())
        .unwrap_or("")
        .to_string();
    if query.is_empty() {
        return Err(call_error("Error: query is required for intent=query"));
    }

    let tool_result = service
        .context_pack(Parameters(ContextPackRequest {
            path: Some(ctx.root_display.clone()),
            query,
            language: None,
            strategy: None,
            limit: None,
            max_chars: Some(ctx.inner_max_chars),
            max_related_per_primary: None,
            include_docs: request.include_docs,
            prefer_code: request.prefer_code,
            related_mode: None,
            auto_index: request.auto_index,
            auto_index_budget_ms: request.auto_index_budget_ms,
            trace: Some(false),
        }))
        .await
        .map_err(|err| call_error(format!("Error: {err}")))?;

    if tool_result.is_error == Some(true) {
        return Err(tool_result);
    }

    let text = tool_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map_or("", |t| t.text.as_str());
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|err| call_error(format!("Error: context_pack returned invalid JSON: {err}")))?;

    sections.push(ReadPackSection::ContextPack { result: value });
    Ok(())
}

async fn handle_onboarding_intent(
    ctx: &ReadPackContext,
    sections: &mut Vec<ReadPackSection>,
) -> ToolResult<()> {
    let onboarding_request = RepoOnboardingPackRequest {
        path: Some(ctx.root_display.clone()),
        map_depth: None,
        map_limit: None,
        doc_paths: None,
        docs_limit: None,
        doc_max_lines: None,
        doc_max_chars: None,
        max_chars: Some(ctx.inner_max_chars),
    };

    let pack =
        compute_repo_onboarding_pack_result(&ctx.root, &ctx.root_display, &onboarding_request)
            .await
            .map_err(|err| call_error(format!("Error: {err:#}")))?;

    sections.push(ReadPackSection::RepoOnboardingPack {
        result: Box::new(pack),
    });
    Ok(())
}

/// Build a one-call semantic reading pack (file slice / grep context / context pack / onboarding).
pub(in crate::tools::dispatch) async fn read_pack(
    service: &ContextFinderService,
    request: ReadPackRequest,
) -> Result<CallToolResult, McpError> {
    let (root, root_display) = match service.resolve_root(request.path.as_deref()).await {
        Ok(value) => value,
        Err(message) => return Ok(call_error(message)),
    };
    let ctx = match build_context(&request, root, root_display) {
        Ok(value) => value,
        Err(result) => return Ok(result),
    };
    let intent = match resolve_intent(&request) {
        Ok(value) => value,
        Err(result) => return Ok(result),
    };

    let timeout_ms = request
        .timeout_ms
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(1_000, MAX_TIMEOUT_MS);
    let meta = service.tool_meta(&ctx.root).await;

    let mut sections: Vec<ReadPackSection> = Vec::new();
    let mut next_actions: Vec<ReadPackNextAction> = Vec::new();

    let handler_future = async {
        match intent {
            ReadPackIntent::Auto => unreachable!("auto intent resolved above"),
            ReadPackIntent::File => {
                handle_file_intent(&ctx, &request, &mut sections, &mut next_actions)
            }
            ReadPackIntent::Grep => {
                handle_grep_intent(&ctx, &request, &mut sections, &mut next_actions).await
            }
            ReadPackIntent::Query => {
                handle_query_intent(service, &ctx, &request, &mut sections).await
            }
            ReadPackIntent::Onboarding => handle_onboarding_intent(&ctx, &mut sections).await,
        }
    };
    let handler_result =
        match tokio::time::timeout(Duration::from_millis(timeout_ms), handler_future).await {
            Ok(result) => result,
            Err(_) => {
                let mut result = ReadPackResult {
                    version: VERSION,
                    intent,
                    root: ctx.root_display.clone(),
                    sections,
                    next_actions,
                    budget: ReadPackBudget {
                        max_chars: ctx.max_chars,
                        used_chars: 0,
                        truncated: true,
                        truncation: Some(ReadPackTruncation::Timeout),
                    },
                    meta: Some(meta),
                };
                apply_meta_to_sections(result.meta.as_ref().unwrap(), &mut result.sections);
                let mut result = match finalize_and_trim(result, ctx.max_chars) {
                    Ok(value) => value,
                    Err(result) => return Ok(result),
                };
                result.budget.truncated = true;
                result.budget.truncation = Some(ReadPackTruncation::Timeout);
                return Ok(CallToolResult::success(vec![Content::text(
                    serde_json::to_string(&result).unwrap_or_default(),
                )]));
            }
        };
    if let Err(result) = handler_result {
        return Ok(result);
    }

    apply_meta_to_sections(&meta, &mut sections);
    let result = ReadPackResult {
        version: VERSION,
        intent,
        root: ctx.root_display.clone(),
        sections,
        next_actions,
        budget: ReadPackBudget {
            max_chars: ctx.max_chars,
            used_chars: 0,
            truncated: false,
            truncation: None,
        },
        meta: Some(meta),
    };

    let mut result = match finalize_and_trim(result, ctx.max_chars) {
        Ok(value) => value,
        Err(result) => return Ok(result),
    };
    ensure_retry_action(&mut result, &ctx, &request, intent);

    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string(&result).unwrap_or_default(),
    )]))
}

#[cfg(test)]
mod tests {
    use super::{build_context, ReadPackRequest};
    use std::path::PathBuf;

    fn base_request() -> ReadPackRequest {
        ReadPackRequest {
            path: Some(".".to_string()),
            intent: None,
            file: None,
            pattern: None,
            query: None,
            file_pattern: None,
            before: None,
            after: None,
            case_sensitive: None,
            start_line: None,
            max_lines: None,
            max_chars: None,
            timeout_ms: None,
            cursor: None,
            prefer_code: None,
            include_docs: None,
            auto_index: None,
            auto_index_budget_ms: None,
        }
    }

    #[test]
    fn build_context_reserves_headroom() {
        let mut request = base_request();
        request.max_chars = Some(20_000);
        let ctx = build_context(&request, PathBuf::from("."), ".".to_string())
            .unwrap_or_else(|_| panic!("build_context should succeed"));
        assert_eq!(ctx.inner_max_chars, 12_000);
    }

    #[test]
    fn build_context_never_exceeds_max_chars() {
        let mut request = base_request();
        request.max_chars = Some(500);
        let ctx = build_context(&request, PathBuf::from("."), ".".to_string())
            .unwrap_or_else(|_| panic!("build_context should succeed"));
        assert_eq!(ctx.inner_max_chars, 1000);
    }
}
