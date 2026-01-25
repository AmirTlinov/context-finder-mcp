use super::super::recall_snippets::{recall_upgrade_to_code_snippets, RecallCodeUpgradeParams};
use super::super::{ProjectFactsResult, ReadPackContext, ReadPackSnippet};
use super::question::RecallQuestionContext;
use std::collections::HashSet;

pub(super) async fn maybe_upgrade_to_code_snippets(
    ctx: &ReadPackContext,
    question: &RecallQuestionContext,
    facts_snapshot: &ProjectFactsResult,
    snippets: &mut Vec<ReadPackSnippet>,
) {
    if question.effective_prefer_code
        && question.structural_intent.is_none()
        && question.ops.is_none()
        && !question.user_directive
        && !question.docs_intent
        && !snippets.is_empty()
    {
        let _ = recall_upgrade_to_code_snippets(
            RecallCodeUpgradeParams {
                ctx,
                facts_snapshot,
                question_tokens: &question.question_tokens,
                snippet_limit: question.snippet_limit,
                snippet_max_chars: question.snippet_max_chars,
                grep_context_lines: question.grep_context_lines,
                include_paths: &question.effective_include_paths,
                exclude_paths: &question.effective_exclude_paths,
                file_pattern: question.effective_file_pattern.as_deref(),
                allow_secrets: question.allow_secrets,
            },
            snippets,
        )
        .await;
    }
}

pub(super) fn dedupe_snippets(
    snippets: Vec<ReadPackSnippet>,
    used_files: &mut HashSet<String>,
) -> Vec<ReadPackSnippet> {
    // Global de-dupe: prefer covering *more files* (breadth) when answering multiple
    // questions in one call. This prevents "README spam" from consuming the entire budget.
    if snippets.len() > 1 {
        let mut unique: Vec<ReadPackSnippet> = Vec::new();
        let mut duplicates: Vec<ReadPackSnippet> = Vec::new();
        for snippet in snippets {
            if used_files.insert(snippet.file.clone()) {
                unique.push(snippet);
            } else {
                duplicates.push(snippet);
            }
        }
        if unique.is_empty() {
            if let Some(first) = duplicates.into_iter().next() {
                unique.push(first);
            }
        }
        unique
    } else if let Some(snippet) = snippets.first() {
        used_files.insert(snippet.file.clone());
        snippets
    } else {
        snippets
    }
}
