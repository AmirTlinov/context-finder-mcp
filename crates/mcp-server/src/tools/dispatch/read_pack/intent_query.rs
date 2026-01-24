use super::super::router::context_pack::context_pack;
use super::super::ContextPackRequest;
use super::candidates::is_disallowed_memory_file;
use super::cursors::{snippet_kind_for_path, trim_chars, trimmed_non_empty_str};
use super::{
    call_error, ReadPackContext, ReadPackRequest, ReadPackSection, ReadPackSnippet, ResponseMode,
    REASON_HALO_CONTEXT_PACK_PRIMARY, REASON_HALO_CONTEXT_PACK_RELATED,
};
use serde_json::Value;

#[derive(Clone, Copy, Debug)]
pub(super) struct QueryIntentPolicy {
    pub(super) allow_secrets: bool,
}

pub(super) async fn handle_query_intent(
    service: &super::ContextFinderService,
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

    let tool_result = context_pack(
        service,
        ContextPackRequest {
            path: Some(ctx.root_display.clone()),
            query,
            language: None,
            strategy: None,
            limit: None,
            max_chars: Some(ctx.inner_max_chars),
            include_paths: request.include_paths.clone(),
            exclude_paths: request.exclude_paths.clone(),
            file_pattern: request.file_pattern.clone(),
            max_related_per_primary: None,
            include_docs: request.include_docs,
            prefer_code: request.prefer_code,
            related_mode: None,
            response_mode: request.response_mode,
            trace: Some(false),
            auto_index: None,
            auto_index_budget_ms: None,
        },
    )
    .await
    .map_err(|err| call_error("internal", format!("Error: {err}")))?;

    if tool_result.is_error == Some(true) {
        return Err(tool_result);
    }

    let mut value: serde_json::Value = tool_result.structured_content.clone().ok_or_else(|| {
        call_error(
            "internal",
            "Error: context_pack returned no structured_content",
        )
    })?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("meta");
        if response_mode != ResponseMode::Full {
            obj.remove("next_actions");
        }
    }

    if response_mode == ResponseMode::Full {
        sections.push(ReadPackSection::ContextPack { result: value });
        return Ok(());
    }

    let snippet_max_chars = (ctx.inner_max_chars / 4)
        .clamp(200, 4_000)
        .min(ctx.inner_max_chars);
    let mut added = 0usize;

    let items = value
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    for role in ["primary", "related"] {
        for item in &items {
            if added >= 5 {
                break;
            }
            if item.get("role").and_then(Value::as_str) != Some(role) {
                continue;
            }
            let Some(file) = item.get("file").and_then(Value::as_str) else {
                continue;
            };
            if !policy.allow_secrets && is_disallowed_memory_file(file) {
                continue;
            }
            let Some(content) = item.get("content").and_then(Value::as_str) else {
                continue;
            };

            let start_line = item.get("start_line").and_then(Value::as_u64).unwrap_or(1) as usize;
            let end_line = item
                .get("end_line")
                .and_then(Value::as_u64)
                .unwrap_or(start_line as u64) as usize;
            let kind = if response_mode == ResponseMode::Minimal {
                None
            } else {
                Some(snippet_kind_for_path(file))
            };
            let reason = match role {
                "primary" => Some(REASON_HALO_CONTEXT_PACK_PRIMARY.to_string()),
                _ => Some(REASON_HALO_CONTEXT_PACK_RELATED.to_string()),
            };
            sections.push(ReadPackSection::Snippet {
                result: ReadPackSnippet {
                    file: file.to_string(),
                    start_line,
                    end_line,
                    content: trim_chars(content, snippet_max_chars),
                    kind,
                    reason,
                    next_cursor: None,
                },
            });
            added += 1;
        }
    }

    if added == 0 {
        // Fallback: emit the raw context_pack JSON (already stripped) so the agent can see "no hits".
        sections.push(ReadPackSection::ContextPack { result: value });
    }
    Ok(())
}
