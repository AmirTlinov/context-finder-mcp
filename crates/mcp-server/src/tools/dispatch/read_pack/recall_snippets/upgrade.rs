use super::super::cursors::limits::MAX_RECALL_SNIPPETS_PER_QUESTION;
use super::super::cursors::snippet_kind_for_path;
use super::super::recall_keywords::recall_keyword_patterns;
use super::super::recall_scoring::{recall_has_code_snippet, score_recall_snippet};
use super::super::ToolResult;
use super::super::{ProjectFactsResult, ReadPackContext, ReadPackSnippet, ReadPackSnippetKind};
use super::grep::{snippets_from_grep_filtered, GrepSnippetParams};
use super::scope::recall_code_scope_candidates;
use std::collections::HashSet;

pub(in crate::tools::dispatch::read_pack) struct RecallCodeUpgradeParams<'a> {
    pub(in crate::tools::dispatch::read_pack) ctx: &'a ReadPackContext,
    pub(in crate::tools::dispatch::read_pack) facts_snapshot: &'a ProjectFactsResult,
    pub(in crate::tools::dispatch::read_pack) question_tokens: &'a [String],
    pub(in crate::tools::dispatch::read_pack) snippet_limit: usize,
    pub(in crate::tools::dispatch::read_pack) snippet_max_chars: usize,
    pub(in crate::tools::dispatch::read_pack) grep_context_lines: usize,
    pub(in crate::tools::dispatch::read_pack) include_paths: &'a [String],
    pub(in crate::tools::dispatch::read_pack) exclude_paths: &'a [String],
    pub(in crate::tools::dispatch::read_pack) file_pattern: Option<&'a str>,
    pub(in crate::tools::dispatch::read_pack) allow_secrets: bool,
}

pub(in crate::tools::dispatch::read_pack) async fn recall_upgrade_to_code_snippets(
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
