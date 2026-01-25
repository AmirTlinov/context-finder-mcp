use super::super::candidates::is_disallowed_memory_file;
use super::super::cursors::{snippet_kind_for_path, trim_chars};
use super::super::{
    ReadPackContext, ReadPackSection, ReadPackSnippet, ResponseMode,
    REASON_HALO_CONTEXT_PACK_PRIMARY, REASON_HALO_CONTEXT_PACK_RELATED,
};
use super::QueryIntentPolicy;
use serde_json::Value;

pub(super) fn append_context_pack_snippets(
    ctx: &ReadPackContext,
    response_mode: ResponseMode,
    policy: QueryIntentPolicy,
    value: &Value,
    sections: &mut Vec<ReadPackSection>,
) -> usize {
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

    added
}
