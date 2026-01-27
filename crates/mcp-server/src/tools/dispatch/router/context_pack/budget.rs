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
        |inner| {
            if inner.items.len() > 1 {
                inner.items.pop();
                inner.budget.dropped_items += 1;
                return true;
            }

            let Some(item) = inner.items.last_mut() else {
                return false;
            };

            // Keep at least one anchor item. Shrink content before giving up.
            if !item.imports.is_empty() {
                item.imports.clear();
                return true;
            }

            if !item.content.is_empty() {
                let target = item.content.len().div_ceil(2);
                let mut cut = target.min(item.content.len());
                while cut > 0 && !item.content.is_char_boundary(cut) {
                    cut = cut.saturating_sub(1);
                }
                if cut == 0 {
                    item.content.clear();
                } else {
                    item.content.truncate(cut);
                }
                return true;
            }

            if item.relationship.is_some() {
                item.relationship = None;
                return true;
            }
            if item.distance.is_some() {
                item.distance = None;
                return true;
            }
            if item.chunk_type.is_some() {
                item.chunk_type = None;
                return true;
            }
            if item.symbol.is_some() {
                item.symbol = None;
                return true;
            }

            false
        },
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
