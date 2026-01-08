use super::super::{
    compute_grep_context_result, GrepContextComputeOptions, GrepContextRequest, ResponseMode,
};
use crate::tools::dispatch::router::grep_context::grep_context_content_budget;
use crate::tools::schemas::content_format::ContentFormat;
use crate::tools::schemas::grep_context::GrepContextHunk;
use regex::RegexBuilder;
use std::path::Path;

pub(super) fn is_semantic_unavailable_error(message: &str) -> bool {
    // Treat these as "semantic temporarily unavailable": callers should fall back to filesystem
    // strategies (grep/text search) instead of failing the tool call.
    //
    // We keep this as a string match because errors cross crate boundaries and are often wrapped.
    message.contains("Index not found")
        || message.contains("No semantic indices available")
        || message.contains("Index is stale")
        || message.contains("Embedding error:")
        || message.contains("Chunk corpus is empty")
        || message.contains("CUDA execution provider")
        || message.contains("CONTEXT_FINDER_ALLOW_CPU=1")
}

pub(super) async fn grep_fallback_hunks(
    root: &Path,
    root_display: &str,
    pattern: &str,
    response_mode: ResponseMode,
    max_hunks: usize,
    max_chars: usize,
) -> anyhow::Result<Vec<GrepContextHunk>> {
    let pattern = pattern.trim();
    anyhow::ensure!(!pattern.is_empty(), "fallback pattern must not be empty");

    let literal = true;
    let case_sensitive = false;
    let before = 2;
    let after = 2;
    let max_matches = 2_000;

    let regex_pattern = regex::escape(pattern);
    let regex = RegexBuilder::new(&regex_pattern)
        .case_insensitive(true)
        .build()?;

    // We don't expose cursor/continuation for fallback: the semantic engine will take over once
    // the index is ready.
    let request = GrepContextRequest {
        path: Some(root_display.to_string()),
        pattern: Some(pattern.to_string()),
        literal: Some(literal),
        file: None,
        file_pattern: None,
        context: None,
        before: Some(before),
        after: Some(after),
        max_matches: Some(max_matches),
        max_hunks: Some(max_hunks),
        max_chars: Some(max_chars),
        case_sensitive: Some(case_sensitive),
        // Low-noise: we only need stable line ranges; the caller decides how to render results.
        format: Some(ContentFormat::Plain),
        response_mode: Some(response_mode),
        allow_secrets: Some(false),
        cursor: None,
    };

    let result = compute_grep_context_result(
        root,
        root_display,
        &request,
        &regex,
        GrepContextComputeOptions {
            case_sensitive,
            before,
            after,
            max_matches,
            max_hunks,
            max_chars,
            content_max_chars: grep_context_content_budget(max_chars, response_mode),
            resume_file: None,
            resume_line: 1,
        },
    )
    .await?;

    Ok(result.hunks)
}
