mod budget;
mod cursor_out;
mod input;
mod question;
mod retrieve;
mod retrieve_grep;
mod retrieve_ops;
mod retrieve_post;
mod retrieve_semantic;
mod retrieve_sources;

use budget::compute_recall_budget;
use cursor_out::write_recall_cursor;
use input::resolve_recall_input;
use question::build_question_context;
use retrieve::collect_recall_snippets;

use super::project_facts::compute_project_facts;
use super::{
    call_error, ContextFinderService, ReadPackContext, ReadPackRecallResult, ReadPackRequest,
    ReadPackSection, ResponseMode, ToolResult,
};
use std::collections::HashSet;

pub(super) async fn handle_recall_intent(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    semantic_index_fresh: bool,
    sections: &mut Vec<ReadPackSection>,
    next_cursor_out: &mut Option<String>,
) -> ToolResult<()> {
    let input = resolve_recall_input(service, ctx, request).await?;
    if input.questions.is_empty() {
        return Err(call_error(
            "missing_field",
            "Error: ask or questions is required for intent=recall",
        ));
    }

    let facts_snapshot = sections
        .iter()
        .find_map(|section| match section {
            ReadPackSection::ProjectFacts { result } => Some(result.clone()),
            _ => None,
        })
        .unwrap_or_else(|| compute_project_facts(&ctx.root));

    let remaining_questions = input
        .questions
        .len()
        .saturating_sub(input.start_index)
        .max(1);
    let budget = compute_recall_budget(ctx, remaining_questions);

    let mut used_files: HashSet<String> = {
        // Per-session working set: avoid repeating the same anchor files across multiple recall
        // calls in one agent session.
        let session = service.session.lock().await;
        session.seen_snippet_files_set_snapshot()
    };

    let mut processed = 0usize;
    let mut next_index = None;

    for (offset, question) in input.questions.iter().enumerate().skip(input.start_index) {
        let question_ctx =
            build_question_context(ctx, question, &input, &budget, semantic_index_fresh);
        let snippets = collect_recall_snippets(
            service,
            ctx,
            response_mode,
            &question_ctx,
            input.topics.as_ref(),
            &facts_snapshot,
            &mut used_files,
        )
        .await;

        sections.push(ReadPackSection::Recall {
            result: ReadPackRecallResult {
                question: question.clone(),
                snippets,
            },
        });
        processed += 1;

        // Pagination guard: keep recall bounded, while letting larger budgets answer more questions.
        if processed >= budget.max_questions_this_call {
            next_index = Some(offset + 1);
            break;
        }
    }

    if let Some(next_question_index) = next_index {
        write_recall_cursor(
            service,
            ctx,
            response_mode,
            &input,
            next_question_index,
            next_cursor_out,
        )
        .await?;
    }

    Ok(())
}
