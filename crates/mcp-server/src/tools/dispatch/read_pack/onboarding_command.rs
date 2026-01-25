use super::super::router::grep_context::grep_context_content_budget;
use super::super::{compute_grep_context_result, GrepContextComputeOptions, GrepContextRequest};
use super::candidates::collect_ops_file_candidates;
use super::cursors::snippet_kind_for_path;
use super::onboarding_topics::{command_grep_pattern, OnboardingTopic};
use super::{
    call_error, ProjectFactsResult, ReadPackContext, ReadPackSection, ReadPackSnippet,
    ResponseMode, ToolResult, REASON_NEEDLE_GREP_HUNK,
};
use crate::tools::schemas::content_format::ContentFormat;
use regex::RegexBuilder;

pub(super) struct CommandSnippetParams<'a> {
    pub ctx: &'a ReadPackContext,
    pub response_mode: ResponseMode,
    pub topic: OnboardingTopic,
    pub facts: &'a ProjectFactsResult,
    pub sections: &'a mut Vec<ReadPackSection>,
}

pub(super) async fn maybe_add_command_snippet(
    params: CommandSnippetParams<'_>,
) -> ToolResult<bool> {
    let CommandSnippetParams {
        ctx,
        response_mode,
        topic,
        facts,
        sections,
    } = params;

    let Some(pattern) = command_grep_pattern(topic, facts) else {
        return Ok(false);
    };

    let grep_max_chars = (ctx.inner_max_chars / 3).clamp(240, 1_200);
    let grep_content_max_chars = grep_context_content_budget(grep_max_chars, response_mode);
    let max_hunks = 1usize;
    let before = 4usize;
    let after = 4usize;

    let regex = RegexBuilder::new(&pattern)
        .case_insensitive(true)
        .build()
        .map_err(|err| call_error("invalid_request", format!("Invalid regex: {err}")))?;

    // 1) Cheap + precise: scan a small shortlist of high-signal "ops" files first.
    let probe_limit = if ctx.inner_max_chars <= 2_000 {
        6usize
    } else {
        10usize
    };
    for rel in collect_ops_file_candidates(&ctx.root)
        .into_iter()
        .take(probe_limit)
    {
        let grep_request = GrepContextRequest {
            path: None,
            pattern: Some(pattern.clone()),
            literal: Some(false),
            file: Some(rel),
            file_pattern: None,
            context: None,
            before: Some(before),
            after: Some(after),
            max_matches: Some(2_000),
            max_hunks: Some(max_hunks),
            max_chars: Some(grep_max_chars),
            case_sensitive: Some(false),
            format: Some(ContentFormat::Plain),
            response_mode: Some(response_mode),
            allow_secrets: Some(false),
            cursor: None,
        };

        let result = compute_grep_context_result(
            &ctx.root,
            &ctx.root_display,
            &grep_request,
            &regex,
            GrepContextComputeOptions {
                case_sensitive: false,
                before,
                after,
                max_matches: 2_000,
                max_hunks,
                max_chars: grep_max_chars,
                content_max_chars: grep_content_max_chars,
                resume_file: None,
                resume_line: 1,
            },
        )
        .await
        .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;

        if let Some(hunk) = result.hunks.first() {
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
                    reason: Some(REASON_NEEDLE_GREP_HUNK.to_string()),
                    next_cursor: None,
                },
            });
            return Ok(true);
        }
    }

    // 2) Fallback: one bounded repo-wide scan if the shortlist didn't hit anything.
    let grep_request = GrepContextRequest {
        path: None,
        pattern: Some(pattern),
        literal: Some(false),
        file: None,
        file_pattern: None,
        context: None,
        before: Some(before),
        after: Some(after),
        max_matches: Some(2_000),
        max_hunks: Some(max_hunks),
        max_chars: Some(grep_max_chars),
        case_sensitive: Some(false),
        format: Some(ContentFormat::Plain),
        response_mode: Some(response_mode),
        allow_secrets: Some(false),
        cursor: None,
    };

    let result = compute_grep_context_result(
        &ctx.root,
        &ctx.root_display,
        &grep_request,
        &regex,
        GrepContextComputeOptions {
            case_sensitive: false,
            before,
            after,
            max_matches: 2_000,
            max_hunks,
            max_chars: grep_max_chars,
            content_max_chars: grep_content_max_chars,
            resume_file: None,
            resume_line: 1,
        },
    )
    .await
    .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;

    if let Some(hunk) = result.hunks.first() {
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
                reason: Some(REASON_NEEDLE_GREP_HUNK.to_string()),
                next_cursor: None,
            },
        });
        return Ok(true);
    }

    Ok(false)
}
