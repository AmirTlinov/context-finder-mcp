use super::super::router::cursor_alias::compact_cursor_alias;
use super::super::router::grep_context::grep_context_content_budget;
use super::super::{
    compute_file_slice_result, compute_grep_context_result, FileSliceRequest,
    GrepContextComputeOptions, GrepContextRequest,
};
use super::candidates::is_disallowed_memory_file;
use super::cursors::{
    snippet_kind_for_path, MAX_RECALL_FILTER_PATHS, MAX_RECALL_SNIPPETS_PER_QUESTION,
};
use super::recall_keywords::recall_keyword_patterns;
use super::recall_paths::{recall_path_allowed, scan_file_pattern_for_include_prefix};
use super::recall_scoring::{recall_has_code_snippet, score_recall_snippet};
use super::{
    call_error, ContextFinderService, ProjectFactsResult, ReadPackContext, ReadPackSnippet,
    ReadPackSnippetKind, ResponseMode, ToolResult, MAX_GREP_MATCHES, REASON_NEEDLE_FILE_SLICE,
    REASON_NEEDLE_GREP_HUNK,
};
use crate::tools::schemas::content_format::ContentFormat;
use regex::RegexBuilder;
use std::collections::HashSet;
use std::path::Path;

fn recall_code_scope_candidates(root: &Path, facts: &ProjectFactsResult) -> Vec<String> {
    // A small, deterministic set of "likely code lives here" roots used as a second-pass scope
    // for precision grep (avoids README/docs-first matches when snippet_limit is tight).
    let mut out: Vec<String> = Vec::new();

    // Prefer project-specific knowledge when available (facts.key_dirs is already bounded).
    for dir in &facts.key_dirs {
        let dir = dir.trim().replace('\\', "/");
        if dir.is_empty() || dir.starts_with('.') {
            continue;
        }
        if matches!(
            dir.as_str(),
            "src"
                | "crates"
                | "packages"
                | "apps"
                | "services"
                | "lib"
                | "libs"
                | "backend"
                | "frontend"
                | "server"
                | "client"
        ) && root.join(&dir).is_dir()
        {
            out.push(dir);
        }
        if out.len() >= 6 {
            break;
        }
    }

    // Fallback: common container directories (covers thin wrappers where key_dirs is noisy).
    if out.is_empty() {
        for dir in [
            "src", "crates", "packages", "apps", "services", "lib", "libs",
        ] {
            if root.join(dir).is_dir() {
                out.push(dir.to_string());
            }
            if out.len() >= 6 {
                break;
            }
        }
    }

    out
}

pub(super) struct GrepSnippetParams {
    pub(super) file: Option<String>,
    pub(super) file_pattern: Option<String>,
    pub(super) before: usize,
    pub(super) after: usize,
    pub(super) max_hunks: usize,
    pub(super) max_chars: usize,
    pub(super) case_sensitive: bool,
    pub(super) allow_secrets: bool,
}

pub(super) struct RecallCodeUpgradeParams<'a> {
    pub(super) ctx: &'a ReadPackContext,
    pub(super) facts_snapshot: &'a ProjectFactsResult,
    pub(super) question_tokens: &'a [String],
    pub(super) snippet_limit: usize,
    pub(super) snippet_max_chars: usize,
    pub(super) grep_context_lines: usize,
    pub(super) include_paths: &'a [String],
    pub(super) exclude_paths: &'a [String],
    pub(super) file_pattern: Option<&'a str>,
    pub(super) allow_secrets: bool,
}

pub(super) async fn recall_upgrade_to_code_snippets(
    params: RecallCodeUpgradeParams<'_>,
    snippets: &mut Vec<ReadPackSnippet>,
) -> ToolResult<()> {
    if snippets.is_empty() || recall_has_code_snippet(snippets) {
        return Ok(());
    }

    let patterns = recall_keyword_patterns(params.question_tokens);
    if patterns.is_empty() {
        return Ok(());
    }

    let probe_hunks = params
        .snippet_limit
        .saturating_mul(8)
        .clamp(2, MAX_RECALL_SNIPPETS_PER_QUESTION);

    let mut found_code: Vec<ReadPackSnippet> = Vec::new();
    for (idx, pattern) in patterns.iter().enumerate() {
        let (found, _cursor) = snippets_from_grep_filtered(
            params.ctx,
            pattern,
            GrepSnippetParams {
                file: None,
                file_pattern: params.file_pattern.map(|p| p.to_string()),
                before: params.grep_context_lines,
                after: params.grep_context_lines,
                max_hunks: probe_hunks,
                max_chars: params.snippet_max_chars,
                case_sensitive: false,
                allow_secrets: params.allow_secrets,
            },
            params.include_paths,
            params.exclude_paths,
            params.file_pattern,
        )
        .await?;

        if found.is_empty() {
            continue;
        }

        if idx == 0 {
            found_code = found;
            break;
        }

        // Second chance: narrow to known code roots to avoid README-first matches.
        let code_scopes = recall_code_scope_candidates(&params.ctx.root, params.facts_snapshot);
        if !code_scopes.is_empty() {
            let (mut scoped, _cursor) = snippets_from_grep_filtered(
                params.ctx,
                pattern,
                GrepSnippetParams {
                    file: None,
                    file_pattern: params.file_pattern.map(|p| p.to_string()),
                    before: params.grep_context_lines,
                    after: params.grep_context_lines,
                    max_hunks: probe_hunks,
                    max_chars: params.snippet_max_chars,
                    case_sensitive: false,
                    allow_secrets: params.allow_secrets,
                },
                &code_scopes,
                params.exclude_paths,
                params.file_pattern,
            )
            .await?;
            scoped.retain(|snippet| {
                snippet_kind_for_path(&snippet.file) == ReadPackSnippetKind::Code
            });
            if !scoped.is_empty() {
                found_code = scoped;
                break;
            }
        }
    }

    if found_code.is_empty() {
        return Ok(());
    }

    let mut seen: HashSet<(String, usize, usize)> = HashSet::new();
    let mut merged: Vec<ReadPackSnippet> = Vec::new();

    for snippet in std::mem::take(snippets)
        .into_iter()
        .chain(found_code.into_iter())
    {
        let key = (snippet.file.clone(), snippet.start_line, snippet.end_line);
        if seen.insert(key) {
            merged.push(snippet);
        }
    }

    merged.sort_by(|a, b| {
        let a_kind = snippet_kind_for_path(&a.file);
        let b_kind = snippet_kind_for_path(&b.file);

        let a_rank = match a_kind {
            ReadPackSnippetKind::Code => 0,
            ReadPackSnippetKind::Config => 1,
            ReadPackSnippetKind::Doc => 2,
        };
        let b_rank = match b_kind {
            ReadPackSnippetKind::Code => 0,
            ReadPackSnippetKind::Config => 1,
            ReadPackSnippetKind::Doc => 2,
        };

        a_rank
            .cmp(&b_rank)
            .then_with(|| {
                score_recall_snippet(params.question_tokens, b)
                    .cmp(&score_recall_snippet(params.question_tokens, a))
            })
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.start_line.cmp(&b.start_line))
            .then_with(|| a.end_line.cmp(&b.end_line))
    });

    merged.truncate(params.snippet_limit.max(1));
    *snippets = merged;
    Ok(())
}

pub(super) async fn snippets_from_grep(
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

pub(super) async fn snippets_from_grep_filtered(
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

#[derive(Clone, Copy, Debug)]
pub(super) struct SnippetFromFileParams {
    pub(super) around_line: Option<usize>,
    pub(super) max_lines: usize,
    pub(super) max_chars: usize,
    pub(super) allow_secrets: bool,
}

pub(super) async fn snippet_from_file(
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
