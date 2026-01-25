use super::super::cursors::DEFAULT_RECALL_SNIPPETS_PER_QUESTION;
use super::super::ReadPackContext;

pub(super) struct RecallBudget {
    pub(super) max_questions_this_call: usize,
    pub(super) per_question_budget: usize,
    pub(super) default_snippets_auto: usize,
    pub(super) default_snippets_fast: usize,
}

pub(super) fn compute_recall_budget(
    ctx: &ReadPackContext,
    remaining_questions: usize,
) -> RecallBudget {
    // Recall is a tight-loop tool and must stay cheap by default.
    //
    // Agent-native behavior: do not expose indexing knobs. Semantic retrieval is used only when
    // the index is already fresh, or when the user explicitly tags a question as `deep`.

    // Memory-UX heuristic: try to answer *more* questions per call by default, but keep snippets
    // small/dry so we fit under budget. This makes recall feel like "project memory" instead of
    // "a sequence of grep calls".
    //
    // We reserve a small slice for the facts section so the questions don't starve the front of
    // the page under mid budgets.
    let reserve_for_facts = match ctx.inner_max_chars {
        0..=2_000 => 260,
        2_001..=6_000 => 420,
        6_001..=12_000 => 650,
        _ => 900,
    };
    let recall_budget_pool = ctx
        .inner_max_chars
        .saturating_sub(reserve_for_facts)
        .max(80)
        .min(ctx.inner_max_chars);

    // Target ~1.4k chars per question under `.context` output. This is intentionally conservative:
    // we'd rather answer more questions with smaller snippets and let the agent "zoom in" with
    // cursor/deep mode.
    let target_per_question = 1_400usize;
    let min_per_question = 650usize;

    let max_questions_by_target = (recall_budget_pool / target_per_question).clamp(1, 8);
    let max_questions_by_min = (recall_budget_pool / min_per_question).max(1);
    let max_questions_this_call = max_questions_by_target
        .min(max_questions_by_min)
        .min(remaining_questions);

    let per_question_budget = recall_budget_pool
        .saturating_div(max_questions_this_call.max(1))
        .max(80);

    // Under smaller per-question budgets, prefer fewer, more informative snippets.
    let default_snippets_auto = if per_question_budget < 1_500 {
        1
    } else if per_question_budget < 3_200 {
        2
    } else {
        DEFAULT_RECALL_SNIPPETS_PER_QUESTION
    };
    let default_snippets_fast = if per_question_budget < 1_500 { 1 } else { 2 };

    RecallBudget {
        max_questions_this_call,
        per_question_budget,
        default_snippets_auto,
        default_snippets_fast,
    }
}
