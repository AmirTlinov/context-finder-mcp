use super::{ReadPackContext, ReadPackIntent, ReadPackRequest};
use super::{ReadPackNextAction, ReadPackResult, DEFAULT_MAX_CHARS, MAX_MAX_CHARS};

mod args;
pub(super) use args::build_retry_args;

pub(super) fn ensure_retry_action(
    result: &mut ReadPackResult,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    intent: ReadPackIntent,
) {
    if !result.budget.truncated || !result.next_actions.is_empty() {
        return;
    }

    let suggested_max_chars = ctx
        .max_chars
        .saturating_mul(2)
        .clamp(DEFAULT_MAX_CHARS, MAX_MAX_CHARS);

    let args = build_retry_args(ctx, request, intent, suggested_max_chars);
    result.next_actions.push(ReadPackNextAction {
        tool: "read_pack".to_string(),
        args,
        reason: "Increase max_chars to get a fuller read_pack payload.".to_string(),
    });
    let _ = super::finalize_read_pack_budget(result);
}
