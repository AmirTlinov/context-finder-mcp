use super::super::recall_keywords::best_keyword_pattern;
use super::super::recall_snippets::{snippets_from_grep_filtered, GrepSnippetParams};
use super::super::{ReadPackContext, ReadPackSnippet};
use super::question::RecallQuestionContext;

pub(super) async fn directive_snippets(
    ctx: &ReadPackContext,
    question: &RecallQuestionContext,
) -> Vec<ReadPackSnippet> {
    if let Some(regex) = question.regex_directive.as_deref() {
        if let Ok((found, _)) = snippets_from_grep_filtered(
            ctx,
            regex,
            GrepSnippetParams {
                file: None,
                file_pattern: question.effective_file_pattern.clone(),
                before: question.grep_context_lines,
                after: question.grep_context_lines,
                max_hunks: question.snippet_limit,
                max_chars: question.snippet_max_chars,
                case_sensitive: true,
                allow_secrets: question.allow_secrets,
            },
            &question.effective_include_paths,
            &question.effective_exclude_paths,
            question.effective_file_pattern.as_deref(),
        )
        .await
        {
            return found;
        }

        let escaped = regex::escape(regex);
        if let Ok((found, _)) = snippets_from_grep_filtered(
            ctx,
            &escaped,
            GrepSnippetParams {
                file: None,
                file_pattern: question.effective_file_pattern.clone(),
                before: question.grep_context_lines,
                after: question.grep_context_lines,
                max_hunks: question.snippet_limit,
                max_chars: question.snippet_max_chars,
                case_sensitive: false,
                allow_secrets: question.allow_secrets,
            },
            &question.effective_include_paths,
            &question.effective_exclude_paths,
            question.effective_file_pattern.as_deref(),
        )
        .await
        {
            return found;
        }
    }

    if let Some(literal) = question.literal_directive.as_deref() {
        let escaped = regex::escape(literal);
        if let Ok((found, _)) = snippets_from_grep_filtered(
            ctx,
            &escaped,
            GrepSnippetParams {
                file: None,
                file_pattern: question.effective_file_pattern.clone(),
                before: question.grep_context_lines,
                after: question.grep_context_lines,
                max_hunks: question.snippet_limit,
                max_chars: question.snippet_max_chars,
                case_sensitive: false,
                allow_secrets: question.allow_secrets,
            },
            &question.effective_include_paths,
            &question.effective_exclude_paths,
            question.effective_file_pattern.as_deref(),
        )
        .await
        {
            return found;
        }
    }

    Vec::new()
}

pub(super) async fn keyword_snippets(
    ctx: &ReadPackContext,
    question: &RecallQuestionContext,
) -> Vec<ReadPackSnippet> {
    let Some(keyword) = best_keyword_pattern(&question.clean_question) else {
        return Vec::new();
    };
    if let Ok((found, _)) = snippets_from_grep_filtered(
        ctx,
        &keyword,
        GrepSnippetParams {
            file: None,
            file_pattern: question.effective_file_pattern.clone(),
            before: question.grep_context_lines,
            after: question.grep_context_lines,
            max_hunks: question.snippet_limit,
            max_chars: question.snippet_max_chars,
            case_sensitive: false,
            allow_secrets: question.allow_secrets,
        },
        &question.effective_include_paths,
        &question.effective_exclude_paths,
        question.effective_file_pattern.as_deref(),
    )
    .await
    {
        return found;
    }

    Vec::new()
}
