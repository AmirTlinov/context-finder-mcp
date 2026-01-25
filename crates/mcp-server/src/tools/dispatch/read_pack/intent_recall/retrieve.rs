use super::super::{
    ContextFinderService, ProjectFactsResult, ReadPackContext, ReadPackSnippet, ResponseMode,
};
use super::question::RecallQuestionContext;
use super::retrieve_grep::{directive_snippets, keyword_snippets};
use super::retrieve_ops::ops_snippets;
use super::retrieve_post::{dedupe_snippets, maybe_upgrade_to_code_snippets};
use super::retrieve_semantic::semantic_snippets;
use super::retrieve_sources::{file_ref_snippets, structural_snippets};
use std::collections::HashSet;

pub(super) async fn collect_recall_snippets(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    response_mode: ResponseMode,
    question: &RecallQuestionContext,
    topics: Option<&Vec<String>>,
    facts_snapshot: &ProjectFactsResult,
    used_files: &mut HashSet<String>,
) -> Vec<ReadPackSnippet> {
    let mut snippets = file_ref_snippets(service, ctx, response_mode, question).await;

    if snippets.is_empty() {
        snippets = structural_snippets(service, ctx, response_mode, question, facts_snapshot).await;
    }

    if snippets.is_empty() {
        snippets = directive_snippets(ctx, question).await;
    }

    if snippets.is_empty() {
        snippets = ops_snippets(service, ctx, response_mode, question).await;
    }

    if snippets.is_empty() {
        snippets = semantic_snippets(service, ctx, response_mode, question, topics).await;
    }

    if snippets.is_empty() && question.ops.is_none() {
        snippets = keyword_snippets(ctx, question).await;
    }

    maybe_upgrade_to_code_snippets(ctx, question, facts_snapshot, &mut snippets).await;

    if snippets.len() > question.snippet_limit {
        snippets.truncate(question.snippet_limit);
    }

    dedupe_snippets(snippets, used_files)
}
