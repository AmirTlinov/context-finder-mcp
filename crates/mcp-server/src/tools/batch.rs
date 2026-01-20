use anyhow::Result;
use context_protocol::{enforce_max_chars, finalize_used_chars, BudgetTruncation, ErrorEnvelope};
use rmcp::model::CallToolResult;

use super::schemas::batch::{
    BatchBudget, BatchItemResult, BatchItemStatus, BatchResult, BatchToolName,
};

pub(super) fn resolve_batch_refs(
    input: serde_json::Value,
    ctx: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    context_batch_ref::resolve_batch_refs(input, ctx)
}

pub(super) fn extract_path_from_input(input: &serde_json::Value) -> Option<String> {
    let serde_json::Value::Object(map) = input else {
        return None;
    };
    map.get("path")
        .or_else(|| map.get("project"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

pub(super) fn prepare_item_input(
    input: serde_json::Value,
    path: Option<&str>,
    tool: BatchToolName,
    remaining_chars: usize,
) -> serde_json::Value {
    let mut input = match input {
        serde_json::Value::Object(map) => serde_json::Value::Object(map),
        _ => serde_json::Value::Object(serde_json::Map::new()),
    };

    if let Some(path) = path {
        if let serde_json::Value::Object(ref mut map) = input {
            map.entry("path".to_string())
                .or_insert_with(|| serde_json::Value::String(path.to_string()));
        }
    }

    if matches!(
        tool,
        BatchToolName::ContextPack
            | BatchToolName::Cat
            | BatchToolName::FileSlice
            | BatchToolName::Ls
            | BatchToolName::Find
            | BatchToolName::ListFiles
            | BatchToolName::GrepContext
            | BatchToolName::Rg
            | BatchToolName::Grep
            | BatchToolName::NotebookPack
            | BatchToolName::NotebookSuggest
            | BatchToolName::RunbookPack
            | BatchToolName::MeaningPack
            | BatchToolName::MeaningFocus
            | BatchToolName::WorktreePack
            | BatchToolName::AtlasPack
            | BatchToolName::EvidenceFetch
    ) {
        if let serde_json::Value::Object(ref mut map) = input {
            if !map.contains_key("max_chars") {
                let cap = remaining_chars.saturating_sub(300).clamp(1, 20_000);
                map.insert(
                    "max_chars".to_string(),
                    serde_json::Value::Number(cap.into()),
                );
            }
        }
    }

    input
}

pub(super) fn parse_tool_result_as_json(
    result: &CallToolResult,
    tool: BatchToolName,
) -> Result<serde_json::Value, String> {
    if result.is_error.unwrap_or(false) {
        if let Some(value) = result.structured_content.clone() {
            if let Some(message) = value
                .get("error")
                .and_then(|err| err.get("message"))
                .and_then(|msg| msg.as_str())
            {
                return Err(message.to_string());
            }
            return Err(value.to_string());
        }
        return Err(extract_tool_text(result).unwrap_or_else(|| "Tool returned error".to_string()));
    }

    if let Some(value) = result.structured_content.clone() {
        return Ok(value);
    }

    let blocks = extract_tool_text_blocks(result);
    if blocks.is_empty() {
        return Err("Tool returned no text content".to_string());
    }

    let mut parsed = Vec::new();
    for block in blocks {
        match serde_json::from_str::<serde_json::Value>(&block) {
            Ok(v) => parsed.push(v),
            Err(err) => {
                return Err(format!("Tool returned non-JSON text content: {err}"));
            }
        }
    }

    match parsed.len() {
        1 => Ok(parsed.into_iter().next().expect("len=1")),
        2 if matches!(tool, BatchToolName::ContextPack) => Ok(serde_json::json!({
            "result": parsed[0],
            "trace": parsed[1],
        })),
        _ => Ok(serde_json::Value::Array(parsed)),
    }
}

fn extract_tool_text(result: &CallToolResult) -> Option<String> {
    let blocks = extract_tool_text_blocks(result);
    if blocks.is_empty() {
        return None;
    }
    Some(blocks.join("\n"))
}

fn extract_tool_text_blocks(result: &CallToolResult) -> Vec<String> {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect()
}

pub(super) fn push_item_or_truncate(
    output: &mut BatchResult,
    item: BatchItemResult,
) -> anyhow::Result<bool> {
    let inserted_id = item.id.clone();
    output.items.push(item);

    let used = match compute_used_chars(output) {
        Ok(used) => used,
        Err(err) => {
            let rejected = output.items.pop().expect("just pushed");
            output.budget.truncated = true;
            output.budget.truncation = Some(BudgetTruncation::MaxChars);
            output.items.push(BatchItemResult {
                id: rejected.id,
                tool: rejected.tool,
                status: BatchItemStatus::Error,
                message: Some(format!("Failed to compute batch budget: {err:#}")),
                error: Some(ErrorEnvelope {
                    code: "internal".to_string(),
                    message: format!("Failed to compute batch budget: {err:#}"),
                    details: None,
                    hint: None,
                    next_actions: Vec::new(),
                }),
                data: serde_json::Value::Null,
            });
            trim_output_to_budget(output)?;
            return Ok(false);
        }
    };

    if used <= output.budget.max_chars {
        output.budget.used_chars = used;
        return Ok(true);
    }

    // Budget exceeded: prefer to keep the newest item by eliding non-text payload
    // (meta/index_state, structured item.data, etc.) before dropping items entirely.
    output.budget.truncated = true;
    output.budget.truncation = Some(BudgetTruncation::MaxChars);
    trim_output_to_budget(output)?;

    // Update used_chars best-effort after truncation.
    if let Ok(used_after) = compute_used_chars(output) {
        output.budget.used_chars = used_after;
    }

    Ok(output.items.iter().any(|item| item.id == inserted_id))
}

pub(super) fn compute_used_chars(output: &BatchResult) -> anyhow::Result<usize> {
    let mut tmp = BatchResult {
        version: output.version,
        items: output.items.clone(),
        budget: BatchBudget {
            max_chars: output.budget.max_chars,
            used_chars: 0,
            truncated: output.budget.truncated,
            truncation: output.budget.truncation.clone(),
        },
        next_actions: output.next_actions.clone(),
        meta: output.meta.clone(),
    };
    finalize_used_chars(&mut tmp, |inner, used| inner.budget.used_chars = used)
}

pub(super) fn trim_output_to_budget(output: &mut BatchResult) -> anyhow::Result<()> {
    let max_chars = output.budget.max_chars;
    let _ = enforce_max_chars(
        output,
        max_chars,
        |inner, used| inner.budget.used_chars = used,
        |inner| {
            inner.budget.truncated = true;
            inner.budget.truncation = Some(BudgetTruncation::MaxChars);
        },
        |inner| {
            // Prefer shrinking non-text fields before dropping items:
            // - `meta.index_state` can be large and is not rendered in the batch text doc.
            // - `item.data` is structured payload; batch text uses captured tool text instead.
            if inner.meta.index_state.is_some() {
                inner.meta.index_state = None;
                return true;
            }

            if let Some(last) = inner.items.last_mut() {
                if !last.data.is_null() {
                    last.data = serde_json::Value::Null;
                    return true;
                }

                let mut changed = false;
                if last.message.is_some() {
                    last.message = None;
                    changed = true;
                }
                if last.error.is_some() {
                    last.error = None;
                    changed = true;
                }
                if changed {
                    return true;
                }
            }

            if !inner.items.is_empty() {
                inner.items.pop();
                return true;
            }

            false
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::schemas::batch::BatchToolName;
    use context_indexer::{IndexSnapshot, IndexState, ReindexAttempt, ToolMeta, Watermark};

    fn make_meta_with_large_index_state(extra: usize) -> ToolMeta {
        let long_error = "x".repeat(extra);
        ToolMeta {
            index_state: Some(IndexState {
                schema_version: 1,
                project_root: Some("/tmp/project".to_string()),
                model_id: "stub".to_string(),
                profile: "quality".to_string(),
                project_watermark: Watermark::Filesystem {
                    computed_at_unix_ms: None,
                    file_count: 1,
                    max_mtime_ms: 0,
                    total_bytes: 0,
                },
                index: IndexSnapshot {
                    exists: true,
                    path: Some("/tmp/index".to_string()),
                    mtime_ms: None,
                    built_at_unix_ms: None,
                    watermark: None,
                },
                stale: true,
                stale_reasons: Vec::new(),
                reindex: Some(ReindexAttempt {
                    attempted: true,
                    performed: false,
                    budget_ms: None,
                    duration_ms: None,
                    result: None,
                    error: Some(long_error),
                }),
            }),
            root_fingerprint: Some(1),
        }
    }

    #[test]
    fn trim_output_elides_meta_and_data_before_dropping_items() {
        let mut output = BatchResult {
            version: 2,
            items: vec![BatchItemResult {
                id: "item".to_string(),
                tool: BatchToolName::RunbookPack,
                status: BatchItemStatus::Ok,
                message: None,
                error: None,
                data: serde_json::json!({ "big": "y".repeat(10_000) }),
            }],
            budget: BatchBudget {
                max_chars: 1_000,
                used_chars: 0,
                truncated: false,
                truncation: None,
            },
            next_actions: Vec::new(),
            meta: make_meta_with_large_index_state(5_000),
        };

        let used_before = compute_used_chars(&output).expect("compute used chars");
        assert!(used_before > output.budget.max_chars);

        trim_output_to_budget(&mut output).expect("trim output to budget");

        let used_after = compute_used_chars(&output).expect("compute used chars after trim");
        assert!(used_after <= output.budget.max_chars);
        assert!(!output.items.is_empty(), "expected at least one item");
        assert!(
            output.meta.index_state.is_none(),
            "expected index_state to be elided"
        );
        assert!(
            output.items[0].data.is_null(),
            "expected item.data to be elided"
        );
        assert!(output.budget.truncated, "expected truncated=true");
    }

    #[test]
    fn push_item_or_truncate_keeps_first_item_by_eliding_payload() {
        let mut output = BatchResult {
            version: 2,
            items: Vec::new(),
            budget: BatchBudget {
                max_chars: 1_000,
                used_chars: 0,
                truncated: false,
                truncation: None,
            },
            next_actions: Vec::new(),
            meta: make_meta_with_large_index_state(5_000),
        };

        let pushed = push_item_or_truncate(
            &mut output,
            BatchItemResult {
                id: "item".to_string(),
                tool: BatchToolName::RunbookPack,
                status: BatchItemStatus::Ok,
                message: None,
                error: None,
                data: serde_json::json!({ "big": "y".repeat(10_000) }),
            },
        )
        .expect("push item");

        assert!(pushed, "expected item to be kept via truncation");
        assert_eq!(output.items.len(), 1);
        assert!(output.budget.truncated);
        assert!(output.meta.index_state.is_none());
        assert!(output.items[0].data.is_null());
    }
}
