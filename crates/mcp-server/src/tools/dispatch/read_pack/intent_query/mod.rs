mod context_pack;
mod snippets;

use context_pack::fetch_context_pack_json;
use snippets::append_context_pack_snippets;

use super::cursors::trimmed_non_empty_str;
use super::{
    call_error, ContextFinderService, ReadPackContext, ReadPackRequest, ReadPackSection,
    ResponseMode,
};

#[derive(Clone, Copy, Debug)]
pub(super) struct QueryIntentPolicy {
    pub(super) allow_secrets: bool,
}

pub(super) async fn handle_query_intent(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    policy: QueryIntentPolicy,
    sections: &mut Vec<ReadPackSection>,
) -> super::ToolResult<()> {
    let query = trimmed_non_empty_str(request.query.as_deref())
        .unwrap_or("")
        .to_string();
    if query.is_empty() {
        return Err(call_error(
            "missing_field",
            "Error: query is required for intent=query",
        ));
    }

    let mut insert_at = sections
        .iter()
        .position(|section| matches!(section, ReadPackSection::ProjectFacts { .. }))
        .map(|idx| idx + 1)
        .unwrap_or(0);
    for memory in
        crate::tools::external_memory::overlays_for_query(&ctx.root, &query, response_mode).await
    {
        sections.insert(
            insert_at,
            ReadPackSection::ExternalMemory { result: memory },
        );
        insert_at = insert_at.saturating_add(1);
    }

    let value = fetch_context_pack_json(service, ctx, request, response_mode, &query).await?;

    if response_mode == ResponseMode::Full {
        sections.push(ReadPackSection::ContextPack { result: value });
        return Ok(());
    }

    let added = append_context_pack_snippets(ctx, response_mode, policy, &value, sections);
    if added == 0 {
        // Fallback: emit the raw context_pack JSON (already stripped) so the agent can see "no hits".
        sections.push(ReadPackSection::ContextPack { result: value });
    }
    Ok(())
}
