use super::super::router::error::invalid_request_with;
use super::recall_trim::trim_recall_sections_for_budget;
use super::retry::{build_retry_args, ensure_retry_action};
use super::{
    call_error, finalize_read_pack_budget, ReadPackBudget, ReadPackContext, ReadPackIntent,
    ReadPackRequest, ReadPackResult, ReadPackTruncation, ResponseMode, ToolResult, MAX_MAX_CHARS,
    MIN_MAX_CHARS,
};
use context_protocol::ToolNextAction;

pub(super) fn trim_project_facts_for_budget(
    mut facts: super::ProjectFactsResult,
    ctx: &ReadPackContext,
    response_mode: ResponseMode,
) -> super::ProjectFactsResult {
    // Under tight budgets, prefer a smaller but still useful facts section so we can always
    // include at least one payload snippet. Deterministic truncation only (no re-ordering).
    let budget = ctx.max_chars;
    let mut cap = if budget <= 1_200 {
        1usize
    } else if budget <= 3_000 {
        2usize
    } else if budget <= 6_000 {
        4usize
    } else {
        usize::MAX
    };
    if response_mode == ResponseMode::Minimal {
        cap = cap.min(2);
    }

    if cap == usize::MAX {
        return facts;
    }

    if budget <= 1_200 {
        // Ultra-tight mode: keep only the most stable, high-signal facts and leave room for at
        // least one snippet.
        truncate_vec(&mut facts.ecosystems, 1);
        truncate_vec(&mut facts.build_tools, 1);
        truncate_vec(&mut facts.ci, 1);
        truncate_vec(&mut facts.contracts, 1);
        // Entry points / config file paths can be long and are better shown as snippets in the
        // memory pack once the budget allows it.
        facts.entry_points.clear();
        facts.key_configs.clear();
        facts.key_dirs.clear();
        facts.modules.clear();
    } else {
        truncate_vec(&mut facts.ecosystems, cap.min(3));
        truncate_vec(&mut facts.build_tools, cap.min(4));
        truncate_vec(&mut facts.ci, cap.min(3));
        truncate_vec(&mut facts.contracts, cap.min(3));
        truncate_vec(&mut facts.key_dirs, cap.min(4));
        truncate_vec(&mut facts.modules, cap.min(6));
        truncate_vec(&mut facts.entry_points, cap.min(4));
        truncate_vec(&mut facts.key_configs, cap.min(6));
    }

    facts
}

fn compute_min_envelope_chars(result: &ReadPackResult) -> ToolResult<usize> {
    let mut tmp = ReadPackResult {
        version: result.version,
        intent: result.intent,
        root: result.root.clone(),
        sections: Vec::new(),
        next_actions: Vec::new(),
        next_cursor: None,
        budget: ReadPackBudget {
            max_chars: result.budget.max_chars,
            used_chars: 0,
            truncated: true,
            truncation: Some(ReadPackTruncation::MaxChars),
        },
        meta: None,
    };
    finalize_read_pack_budget(&mut tmp)
        .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;
    Ok(tmp.budget.used_chars)
}

pub(super) fn finalize_and_trim(
    mut result: ReadPackResult,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    intent: ReadPackIntent,
    response_mode: ResponseMode,
) -> ToolResult<ReadPackResult> {
    finalize_read_pack_budget(&mut result)
        .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;

    // Cursor-first UX: the presence of a continuation cursor means the response is incomplete
    // (paginated), even if we still fit under `max_chars`. Surface this deterministically via the
    // budget envelope so agents can rely on `truncated` as a single signal for "there is more".
    if result.next_cursor.is_some() && !result.budget.truncated {
        result.budget.truncated = true;
        if result.budget.truncation.is_none() {
            result.budget.truncation = Some(ReadPackTruncation::MaxItems);
        }
        // We mutated the envelope after computing `used_chars`; recompute so trimming decisions
        // stay correct under tight budgets.
        let _ = finalize_read_pack_budget(&mut result);
    }

    if result.budget.used_chars <= ctx.max_chars {
        return Ok(result);
    }

    result.budget.truncated = true;
    // If we exceeded max_chars, this is the dominant truncation reason even when we also have a
    // pagination cursor (max_items).
    result.budget.truncation = Some(ReadPackTruncation::MaxChars);

    // Recall pages should degrade by dropping snippets before dropping entire questions.
    if matches!(intent, ReadPackIntent::Recall) {
        let _ = trim_recall_sections_for_budget(&mut result, ctx.max_chars);
        let _ = finalize_read_pack_budget(&mut result);
        if result.budget.used_chars <= ctx.max_chars {
            return Ok(result);
        }
    }

    while result.budget.used_chars > ctx.max_chars && result.sections.len() > 1 {
        result.sections.pop();
        result.next_actions.clear();
        let _ = finalize_read_pack_budget(&mut result);
    }

    if result.budget.used_chars > ctx.max_chars {
        if !result.next_actions.is_empty() {
            result.next_actions.clear();
            let _ = finalize_read_pack_budget(&mut result);
        }
        if response_mode != ResponseMode::Full
            && result
                .meta
                .as_ref()
                .is_some_and(|meta| meta.index_state.is_some())
        {
            // Under very tight budgets, drop heavy diagnostics before sacrificing payload.
            result.meta = None;
            let _ = finalize_read_pack_budget(&mut result);
        }
        // Under extreme budgets we prefer to keep the continuation cursor (cheap) even if we must
        // drop all payload sections (expensive). This preserves an agent's tight-loop UX: the agent
        // can continue with a larger budget without losing pagination state.
        if result.budget.used_chars > ctx.max_chars {
            result.sections.clear();
            let _ = finalize_read_pack_budget(&mut result);
        }
        if result.budget.used_chars > ctx.max_chars && result.next_cursor.is_some() {
            result.next_cursor = None;
            let _ = finalize_read_pack_budget(&mut result);
        }
        if result.budget.used_chars > ctx.max_chars {
            let min_chars = compute_min_envelope_chars(&result)?;
            let suggested_max_chars = min_chars
                .max(ctx.max_chars.saturating_mul(2))
                .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);
            let retry_args = build_retry_args(ctx, request, intent, suggested_max_chars);
            return Err(invalid_request_with(
                format!("max_chars too small for read_pack response (min_chars={min_chars})"),
                Some(format!("Increase max_chars to at least {min_chars}.")),
                vec![ToolNextAction {
                    tool: "read_pack".to_string(),
                    args: retry_args,
                    reason: format!("Retry read_pack with max_chars >= {min_chars}."),
                }],
            ));
        }
    }

    if response_mode == ResponseMode::Full
        && result.budget.truncated
        && result.next_actions.is_empty()
        && result.next_cursor.is_none()
        && matches!(
            result.budget.truncation,
            Some(ReadPackTruncation::MaxChars | ReadPackTruncation::Timeout)
        )
    {
        ensure_retry_action(&mut result, ctx, request, intent);
        let _ = finalize_read_pack_budget(&mut result);
        if result.budget.used_chars > ctx.max_chars {
            result.next_actions.clear();
            let _ = finalize_read_pack_budget(&mut result);
        }
    }

    Ok(result)
}

fn truncate_vec<T>(values: &mut Vec<T>, max: usize) {
    if values.len() > max {
        values.truncate(max);
    }
}
