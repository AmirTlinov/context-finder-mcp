#[path = "budget_shrink.rs"]
mod budget_shrink;

use self::budget_shrink::shrink_context_pack_output;
use super::ToolResult;
use context_protocol::{enforce_max_chars, BudgetTruncation};
use context_search::ContextPackOutput;

pub(super) fn enforce_context_pack_budget(output: &mut ContextPackOutput) -> ToolResult<()> {
    let max_chars = output.budget.max_chars;
    let res = enforce_max_chars(
        output,
        max_chars,
        |inner, used| inner.budget.used_chars = used,
        |inner| {
            inner.budget.truncated = true;
            if inner.budget.truncation.is_none() {
                inner.budget.truncation = Some(BudgetTruncation::MaxChars);
            }
        },
        shrink_context_pack_output,
    );
    match res {
        Ok(_) => Ok(()),
        Err(_err) => {
            // Fail-soft: under extremely small budgets the envelope can dominate even a
            // single-item pack. Prefer returning an empty (but valid) pack over erroring.
            output.items.clear();
            output.budget.truncated = true;
            if output.budget.truncation.is_none() {
                output.budget.truncation = Some(BudgetTruncation::MaxChars);
            }
            Ok(())
        }
    }
}
