use crate::tools::context_doc::ContextDocBuilder;
use context_search::{count_anchor_hits, detect_primary_anchor, ContextPackOutput};

use crate::tools::dispatch::ResponseMode;
use context_indexer::AnchorPolicy;

fn retrieval_mode_label(
    output: &ContextPackOutput,
    semantic_disabled_reason: Option<&str>,
) -> &'static str {
    if let Some(trust) = output.meta.trust.as_ref() {
        if let Some(mode) = trust.retrieval_mode {
            return match mode {
                context_indexer::RetrievalMode::Semantic => "semantic",
                context_indexer::RetrievalMode::Hybrid => "hybrid",
                context_indexer::RetrievalMode::Lexical => "lexical",
            };
        }
    }
    if semantic_disabled_reason.is_some() {
        return "lexical";
    }
    if output
        .items
        .iter()
        .any(|item| item.id.starts_with("lexical:"))
    {
        return "lexical";
    }
    "hybrid"
}

fn fallback_used_flag(output: &ContextPackOutput, semantic_disabled_reason: Option<&str>) -> bool {
    if let Some(trust) = output.meta.trust.as_ref() {
        if let Some(flag) = trust.fallback_used {
            return flag;
        }
    }
    semantic_disabled_reason.is_some()
        || output
            .items
            .iter()
            .any(|item| item.id.starts_with("lexical:"))
}

fn index_state_label(index_state: Option<&context_indexer::IndexState>) -> &'static str {
    let Some(state) = index_state else {
        return "unknown";
    };
    if !state.index.exists {
        return "missing";
    }
    if state.stale {
        return "stale";
    }
    "ok"
}

fn anchor_stats(output: &ContextPackOutput) -> (bool, usize) {
    let Some(anchor) = detect_primary_anchor(&output.query) else {
        return (false, 0);
    };
    (true, count_anchor_hits(&output.items, &anchor))
}

pub(in crate::tools::dispatch::router::context_pack) fn maybe_push_trust_micro_meta(
    doc: &mut ContextDocBuilder,
    response_mode: ResponseMode,
    output: &ContextPackOutput,
    semantic_disabled_reason: Option<&str>,
) {
    let (anchor_detected, anchor_hits) = anchor_stats(output);
    let fallback_used = fallback_used_flag(output, semantic_disabled_reason);
    let retrieval_mode = retrieval_mode_label(output, semantic_disabled_reason);

    let show = match response_mode {
        ResponseMode::Full | ResponseMode::Facts => true,
        ResponseMode::Minimal => {
            output.items.is_empty()
                || output.budget.truncated
                || fallback_used
                || (anchor_detected && anchor_hits == 0)
        }
    };
    if !show {
        return;
    }

    let index_state = index_state_label(output.meta.index_state.as_ref());
    doc.push_note(&format!(
        "retrieval_mode={retrieval_mode} fallback_used={fallback_used} index_state={index_state}"
    ));
    doc.push_note(&format!(
        "anchor_detected={anchor_detected} anchor_hits={anchor_hits}"
    ));
}

fn anchor_policy_label(policy: Option<AnchorPolicy>) -> &'static str {
    match policy {
        Some(AnchorPolicy::Auto) => "auto",
        Some(AnchorPolicy::Off) => "off",
        None => "unknown",
    }
}

pub(in crate::tools::dispatch::router::context_pack) fn push_v2_envelope(
    doc: &mut ContextDocBuilder,
    output: &ContextPackOutput,
    semantic_disabled_reason: Option<&str>,
) {
    // PROVENANCE (brief + always present in v2).
    doc.push_note("PROVENANCE:");
    doc.push_root_fingerprint(output.meta.root_fingerprint);
    let retrieval_mode = retrieval_mode_label(output, semantic_disabled_reason);
    let fallback_used = fallback_used_flag(output, semantic_disabled_reason);
    let index_state = index_state_label(output.meta.index_state.as_ref());
    doc.push_note(&format!(
        "retrieval_mode={retrieval_mode} fallback_used={fallback_used} index_state={index_state}"
    ));
    if let Some(reason) = semantic_disabled_reason {
        doc.push_note(&format!("semantic_disabled_reason={reason}"));
    }

    // GUARANTEES (minimal).
    doc.push_note("GUARANTEES:");
    let primary_anchor = detect_primary_anchor(&output.query);
    let (anchor_detected, anchor_hits) = anchor_stats(output);
    let anchor_policy = output.meta.trust.as_ref().and_then(|t| t.anchor_policy);
    let anchor_policy_label = anchor_policy_label(anchor_policy);
    let anchor_fail_closed = anchor_policy != Some(AnchorPolicy::Off) && anchor_detected;

    if let Some(anchor) = primary_anchor {
        let anchor_not_found = output
            .meta
            .trust
            .as_ref()
            .and_then(|t| t.anchor_not_found)
            .unwrap_or(anchor_hits == 0 && output.items.is_empty());
        doc.push_note(&format!(
            "anchor_fail_closed={anchor_fail_closed} anchor_policy={anchor_policy_label} anchor_kind={:?} anchor_primary={} anchor_hits={anchor_hits} anchor_not_found={anchor_not_found}",
            anchor.kind, anchor.normalized
        ));
    } else {
        doc.push_note(&format!(
            "anchor_fail_closed={anchor_fail_closed} anchor_policy={anchor_policy_label} anchor_detected={anchor_detected}"
        ));
    }

    if output.budget.truncated {
        let trunc = output
            .budget
            .truncation
            .as_ref()
            .map(|t| format!("{t:?}"))
            .unwrap_or_else(|| "unknown".to_string());
        doc.push_note(&format!(
            "truncation=true reason={trunc} dropped_items={}",
            output.budget.dropped_items
        ));
    } else {
        doc.push_note("truncation=false");
    }
}

pub(in crate::tools::dispatch::router::context_pack) fn push_next_actions(
    doc: &mut ContextDocBuilder,
    output: &ContextPackOutput,
) {
    if output.next_actions.is_empty() {
        return;
    }
    doc.push_note("next_actions:");
    let mut shown = 0usize;
    for action in output.next_actions.iter().take(3) {
        shown += 1;
        let args = serde_json::to_string(&action.args).unwrap_or_else(|_| "{}".to_string());
        doc.push_line(&format!(" - {} {args}", action.tool));
    }
    if output.next_actions.len() > shown {
        doc.push_line(&format!(
            " - … (showing {shown} of {})",
            output.next_actions.len()
        ));
    }
    doc.push_blank();
}

pub(in crate::tools::dispatch::router::context_pack) fn push_next_actions_v2(
    doc: &mut ContextDocBuilder,
    output: &ContextPackOutput,
) {
    doc.push_note("NEXT:");
    if output.next_actions.is_empty() {
        doc.push_line(" - (none)");
        doc.push_blank();
        return;
    }
    let mut shown = 0usize;
    for action in output.next_actions.iter().take(3) {
        shown += 1;
        let args = serde_json::to_string(&action.args).unwrap_or_else(|_| "{}".to_string());
        doc.push_line(&format!(" - {} {args}", action.tool));
    }
    if output.next_actions.len() > shown {
        doc.push_line(&format!(
            " - … (showing {shown} of {})",
            output.next_actions.len()
        ));
    }
    doc.push_blank();
}
