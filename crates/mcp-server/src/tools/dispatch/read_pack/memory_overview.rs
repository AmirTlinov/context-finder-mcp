use super::super::router::overview::overview;
use super::super::{OverviewRequest, OverviewResult};
use super::cursors::trimmed_non_empty_str;
use super::{
    ContextFinderService, ReadPackContext, ReadPackRequest, ReadPackSection, ResponseMode,
};

pub(super) async fn maybe_add_overview(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    response_mode: ResponseMode,
    sections: &mut Vec<ReadPackSection>,
) {
    if response_mode != ResponseMode::Full {
        return;
    }

    // Memory-pack default UX is "native-fast": avoid graph/index-heavy work unless we already
    // have a fresh semantic index (otherwise overview would trigger reindex work).
    let meta = service.tool_meta(&ctx.root).await;
    let has_fresh_index = meta
        .index_state
        .as_ref()
        .is_some_and(|state| state.index.exists && !state.stale);

    if !has_fresh_index {
        return;
    }

    let overview_request = OverviewRequest {
        path: Some(ctx.root_display.clone()),
        language: None,
        response_mode: None,
        auto_index: None,
        auto_index_budget_ms: None,
    };

    if let Ok(tool_result) = overview(service, overview_request).await {
        if tool_result.is_error != Some(true) {
            if let Some(value) = tool_result.structured_content.clone() {
                if let Ok(overview) = serde_json::from_value::<OverviewResult>(value) {
                    sections.push(ReadPackSection::Overview { result: overview });
                }
            }
        }
    }
}

pub(super) async fn insert_external_memory_overlays(
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    sections: &mut Vec<ReadPackSection>,
) {
    // Include recent Codex CLI worklog context (project-scoped, bounded, deduped) on the initial
    // memory pack entry. Cursor continuations should stay payload-focused and avoid repeating
    // overlays.
    if trimmed_non_empty_str(request.cursor.as_deref()).is_some() {
        return;
    }

    let overlays = crate::tools::external_memory::overlays_recent(&ctx.root, response_mode).await;
    if overlays.is_empty() {
        return;
    }

    let mut insert_at = sections
        .iter()
        .position(|section| matches!(section, ReadPackSection::ProjectFacts { .. }))
        .map(|idx| idx + 1)
        .unwrap_or(0);

    for memory in overlays {
        sections.insert(
            insert_at,
            ReadPackSection::ExternalMemory { result: memory },
        );
        insert_at = insert_at.saturating_add(1);
    }
}
