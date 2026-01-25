use super::super::super::router::grep_context::grep_context_content_budget;
use super::super::super::{
    compute_grep_context_result, GrepContextComputeOptions, GrepContextRequest,
};
use super::super::cursors::{
    snippet_kind_for_path, MAX_RECALL_FILTER_PATHS, MAX_RECALL_SNIPPETS_PER_QUESTION,
};
use super::super::recall_paths::{recall_path_allowed, scan_file_pattern_for_include_prefix};
use super::super::{
    call_error, ContextFinderService, ReadPackContext, ReadPackSnippet, ResponseMode, ToolResult,
    MAX_GREP_MATCHES, REASON_NEEDLE_GREP_HUNK,
};
use crate::tools::schemas::content_format::ContentFormat;
use regex::RegexBuilder;
use std::collections::HashSet;

pub(in crate::tools::dispatch::read_pack) struct GrepSnippetParams {
    pub(in crate::tools::dispatch::read_pack) file: Option<String>,
    pub(in crate::tools::dispatch::read_pack) file_pattern: Option<String>,
    pub(in crate::tools::dispatch::read_pack) before: usize,
    pub(in crate::tools::dispatch::read_pack) after: usize,
    pub(in crate::tools::dispatch::read_pack) max_hunks: usize,
    pub(in crate::tools::dispatch::read_pack) max_chars: usize,
    pub(in crate::tools::dispatch::read_pack) case_sensitive: bool,
    pub(in crate::tools::dispatch::read_pack) allow_secrets: bool,
}

pub(in crate::tools::dispatch::read_pack) async fn snippets_from_grep(
    ctx: &ReadPackContext,
    pattern: &str,
    params: GrepSnippetParams,
) -> ToolResult<(Vec<ReadPackSnippet>, Option<String>)> {
    let max_hunks = params.max_hunks;
    let regex = RegexBuilder::new(pattern)
        .case_insensitive(!params.case_sensitive)
        .build()
        .map_err(|err| call_error("invalid_request", format!("Invalid regex: {err}")))?;
    let grep_request = GrepContextRequest {
        path: None,
        pattern: Some(pattern.to_string()),
        literal: Some(false),
        file: params.file,
        file_pattern: params.file_pattern,
        context: None,
        before: Some(params.before),
        after: Some(params.after),
        max_matches: Some(MAX_GREP_MATCHES.min(5_000)),
        max_hunks: Some(params.max_hunks),
        max_chars: Some(params.max_chars),
        case_sensitive: Some(params.case_sensitive),
        format: Some(ContentFormat::Plain),
        // Internal: these hunks are re-packed into read_pack snippets, so we can treat them as
        // "minimal" to maximize payload (grep_context's Facts mode reserves a lot of envelope
        // headroom that doesn't apply here).
        response_mode: Some(ResponseMode::Minimal),
        allow_secrets: Some(params.allow_secrets),
        cursor: None,
    };

    let result = compute_grep_context_result(
        &ctx.root,
        &ctx.root_display,
        &grep_request,
        &regex,
        GrepContextComputeOptions {
            case_sensitive: params.case_sensitive,
            before: params.before,
            after: params.after,
            max_matches: MAX_GREP_MATCHES.min(5_000),
            max_hunks: params.max_hunks,
            max_chars: params.max_chars,
            content_max_chars: grep_context_content_budget(params.max_chars, ResponseMode::Minimal),
            resume_file: None,
            resume_line: 1,
        },
    )
    .await
    .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;

    let mut snippets = Vec::new();
    for hunk in result.hunks.iter().take(max_hunks) {
        snippets.push(ReadPackSnippet {
            file: hunk.file.clone(),
            start_line: hunk.start_line,
            end_line: hunk.end_line,
            content: hunk.content.clone(),
            kind: Some(snippet_kind_for_path(&hunk.file)),
            reason: Some(REASON_NEEDLE_GREP_HUNK.to_string()),
            next_cursor: None,
        });
    }
    Ok((snippets, result.next_cursor.clone()))
}

pub(in crate::tools::dispatch::read_pack) async fn snippets_from_grep_filtered(
    ctx: &ReadPackContext,
    pattern: &str,
    params: GrepSnippetParams,
    include_paths: &[String],
    exclude_paths: &[String],
    required_file_pattern: Option<&str>,
) -> ToolResult<(Vec<ReadPackSnippet>, Option<String>)> {
    let max_hunks = params.max_hunks.min(MAX_RECALL_SNIPPETS_PER_QUESTION);
    if let Some(file) = params.file.as_ref() {
        if !recall_path_allowed(file, include_paths, exclude_paths) {
            return Ok((Vec::new(), None));
        }
    }

    if include_paths.is_empty() {
        let (mut snippets, cursor) = snippets_from_grep(ctx, pattern, params).await?;
        snippets.retain(|snippet| {
            recall_path_allowed(&snippet.file, include_paths, exclude_paths)
                && ContextFinderService::matches_file_pattern(&snippet.file, required_file_pattern)
        });
        return Ok((snippets, cursor));
    }

    let mut out: Vec<ReadPackSnippet> = Vec::new();
    let mut seen = HashSet::new();

    for prefix in include_paths.iter().take(MAX_RECALL_FILTER_PATHS) {
        let Some(scan_pattern) = scan_file_pattern_for_include_prefix(&ctx.root, prefix) else {
            continue;
        };

        let (snippets, _cursor) = snippets_from_grep(
            ctx,
            pattern,
            GrepSnippetParams {
                file: params.file.clone(),
                file_pattern: Some(scan_pattern),
                before: params.before,
                after: params.after,
                max_hunks: params.max_hunks,
                max_chars: params.max_chars,
                case_sensitive: params.case_sensitive,
                allow_secrets: params.allow_secrets,
            },
        )
        .await?;

        for snippet in snippets {
            if out.len() >= max_hunks {
                break;
            }
            if !recall_path_allowed(&snippet.file, include_paths, exclude_paths) {
                continue;
            }
            if !ContextFinderService::matches_file_pattern(&snippet.file, required_file_pattern) {
                continue;
            }
            let key = (snippet.file.clone(), snippet.start_line, snippet.end_line);
            if seen.insert(key) {
                out.push(snippet);
            }
        }

        if out.len() >= max_hunks {
            break;
        }
    }

    Ok((out, None))
}
