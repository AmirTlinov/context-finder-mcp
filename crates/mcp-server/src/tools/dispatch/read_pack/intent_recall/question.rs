use super::super::cursors::limits::{MAX_RECALL_FILTER_PATHS, MAX_RECALL_SNIPPETS_PER_QUESTION};
use super::super::recall::extract_existing_file_ref;
use super::super::recall::recall_structural_intent;
use super::super::recall::OpsIntent;
use super::super::recall::RecallStructuralIntent;
use super::super::recall_directives::{
    parse_recall_literal_directive, parse_recall_question_directives, parse_recall_regex_directive,
    recall_question_policy, RecallQuestionMode,
};
use super::super::recall_keywords::recall_question_tokens;
use super::super::recall_ops::ops_intent;
use super::super::recall_paths::merge_recall_prefix_lists;
use super::super::ReadPackContext;
use super::budget::RecallBudget;
use super::input::RecallInput;
use context_search::QueryClassifier;

pub(super) struct RecallQuestionContext {
    pub(super) clean_question: String,
    pub(super) question_mode: RecallQuestionMode,
    pub(super) snippet_limit: usize,
    pub(super) grep_context_lines: usize,
    pub(super) snippet_max_chars: usize,
    pub(super) snippet_max_lines: usize,
    pub(super) effective_include_paths: Vec<String>,
    pub(super) effective_exclude_paths: Vec<String>,
    pub(super) effective_file_pattern: Option<String>,
    pub(super) effective_prefer_code: bool,
    pub(super) allow_semantic: bool,
    pub(super) user_directive: bool,
    pub(super) structural_intent: Option<RecallStructuralIntent>,
    pub(super) ops: Option<OpsIntent>,
    pub(super) docs_intent: bool,
    pub(super) question_tokens: Vec<String>,
    pub(super) regex_directive: Option<String>,
    pub(super) literal_directive: Option<String>,
    pub(super) file_ref: Option<(String, Option<usize>)>,
    pub(super) allow_secrets: bool,
    pub(super) include_docs: Option<bool>,
    pub(super) prefer_code: Option<bool>,
}

pub(super) fn build_question_context(
    ctx: &ReadPackContext,
    question: &str,
    input: &RecallInput,
    budget: &RecallBudget,
    semantic_index_fresh: bool,
) -> RecallQuestionContext {
    let (clean_question, directives) = parse_recall_question_directives(question, &ctx.root);
    let clean_question = if clean_question.is_empty() {
        question.to_string()
    } else {
        clean_question
    };
    let regex_directive = parse_recall_regex_directive(&clean_question);
    let literal_directive = parse_recall_literal_directive(&clean_question);
    let user_directive = regex_directive.is_some() || literal_directive.is_some();
    let structural_intent = if user_directive {
        None
    } else {
        recall_structural_intent(&clean_question)
    };
    let ops = ops_intent(&clean_question);
    let docs_intent = QueryClassifier::is_docs_intent(&clean_question);
    let question_tokens = recall_question_tokens(&clean_question);
    let effective_prefer_code = input.prefer_code.unwrap_or(!docs_intent);

    let question_mode = directives.mode;
    let base_snippet_limit = match question_mode {
        RecallQuestionMode::Fast => budget.default_snippets_fast,
        RecallQuestionMode::Deep => MAX_RECALL_SNIPPETS_PER_QUESTION,
        RecallQuestionMode::Auto => budget.default_snippets_auto,
    };
    let snippet_limit = directives
        .snippet_limit
        .unwrap_or(base_snippet_limit)
        .clamp(1, MAX_RECALL_SNIPPETS_PER_QUESTION);
    let grep_context_lines = directives.grep_context.unwrap_or(12);

    let snippet_max_chars = budget
        .per_question_budget
        .saturating_div(snippet_limit.max(1))
        .clamp(40, 4_000)
        .min(ctx.inner_max_chars);
    let snippet_max_chars = match question_mode {
        RecallQuestionMode::Deep => snippet_max_chars,
        _ => snippet_max_chars.min(1_200),
    };
    let snippet_max_lines = if snippet_max_chars < 600 {
        60
    } else if snippet_max_chars < 1_200 {
        90
    } else {
        120
    };

    let policy = recall_question_policy(question_mode, semantic_index_fresh);
    let allow_semantic = policy.allow_semantic;

    let effective_include_paths = merge_recall_prefix_lists(
        &input.include_paths,
        &directives.include_paths,
        MAX_RECALL_FILTER_PATHS,
    );
    let effective_exclude_paths = merge_recall_prefix_lists(
        &input.exclude_paths,
        &directives.exclude_paths,
        MAX_RECALL_FILTER_PATHS,
    );

    let effective_file_pattern = directives
        .file_pattern
        .clone()
        .or_else(|| input.file_pattern.clone());

    let explicit_file_ref = directives.file_ref.clone();
    let detected_file_ref =
        extract_existing_file_ref(&clean_question, &ctx.root, input.allow_secrets);
    let file_ref = explicit_file_ref.or(detected_file_ref);

    RecallQuestionContext {
        clean_question,
        question_mode,
        snippet_limit,
        grep_context_lines,
        snippet_max_chars,
        snippet_max_lines,
        effective_include_paths,
        effective_exclude_paths,
        effective_file_pattern,
        effective_prefer_code,
        allow_semantic,
        user_directive,
        structural_intent,
        ops,
        docs_intent,
        question_tokens,
        regex_directive,
        literal_directive,
        file_ref,
        allow_secrets: input.allow_secrets,
        include_docs: input.include_docs,
        prefer_code: input.prefer_code,
    }
}
