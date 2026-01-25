use super::super::anchor_scan::best_anchor_line_for_kind;
use super::super::cursors::snippet_kind_for_path;
use super::super::recall_paths::recall_path_allowed;
use super::super::recall_snippets::{snippet_from_file, SnippetFromFileParams};
use super::super::recall_structural::recall_structural_candidates;
use super::super::{
    ContextFinderService, ProjectFactsResult, ReadPackContext, ReadPackSnippet, ResponseMode,
};
use super::question::RecallQuestionContext;

pub(super) async fn file_ref_snippets(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    response_mode: ResponseMode,
    question: &RecallQuestionContext,
) -> Vec<ReadPackSnippet> {
    let mut snippets: Vec<ReadPackSnippet> = Vec::new();
    if let Some((file, line)) = question.file_ref.clone() {
        if let Ok(snippet) = snippet_from_file(
            service,
            ctx,
            &file,
            SnippetFromFileParams {
                around_line: line,
                max_lines: question.snippet_max_lines,
                max_chars: question.snippet_max_chars,
                allow_secrets: question.allow_secrets,
            },
            response_mode,
        )
        .await
        {
            snippets.push(snippet);
        }
    }
    snippets
}

pub(super) async fn structural_snippets(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    response_mode: ResponseMode,
    question: &RecallQuestionContext,
    facts_snapshot: &ProjectFactsResult,
) -> Vec<ReadPackSnippet> {
    let Some(structural_intent) = question.structural_intent else {
        return Vec::new();
    };

    let mut snippets: Vec<ReadPackSnippet> = Vec::new();
    let candidates = recall_structural_candidates(structural_intent, &ctx.root, facts_snapshot);
    for file in candidates.into_iter().take(32) {
        if !recall_path_allowed(
            &file,
            &question.effective_include_paths,
            &question.effective_exclude_paths,
        ) {
            continue;
        }
        if !ContextFinderService::matches_file_pattern(
            &file,
            question.effective_file_pattern.as_deref(),
        ) {
            continue;
        }

        let kind = snippet_kind_for_path(&file);
        let anchor = best_anchor_line_for_kind(&ctx.root, &file, kind);

        if let Ok(snippet) = snippet_from_file(
            service,
            ctx,
            &file,
            SnippetFromFileParams {
                around_line: anchor,
                max_lines: question.snippet_max_lines,
                max_chars: question.snippet_max_chars,
                allow_secrets: question.allow_secrets,
            },
            response_mode,
        )
        .await
        {
            snippets.push(snippet);
        }

        if snippets.len() >= question.snippet_limit {
            break;
        }
    }

    snippets
}
