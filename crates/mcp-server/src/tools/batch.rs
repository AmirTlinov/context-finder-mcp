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
            | BatchToolName::FileSlice
            | BatchToolName::ListFiles
            | BatchToolName::GrepContext
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

    if used > output.budget.max_chars {
        let rejected = output.items.pop().expect("just pushed");
        output.budget.truncated = true;
        output.budget.truncation = Some(BudgetTruncation::MaxChars);

        if output.items.is_empty() {
            let message = format!(
                "Batch budget exceeded (max_chars={}). Reduce payload sizes or raise max_chars.",
                output.budget.max_chars
            );
            output.items.push(BatchItemResult {
                id: rejected.id,
                tool: rejected.tool,
                status: BatchItemStatus::Error,
                message: Some(message.clone()),
                error: Some(ErrorEnvelope {
                    code: "invalid_request".to_string(),
                    message,
                    details: None,
                    hint: None,
                    next_actions: Vec::new(),
                }),
                data: serde_json::Value::Null,
            });
            if let Ok(over) = compute_used_chars(output) {
                if over > output.budget.max_chars {
                    if let Some(last) = output.items.last_mut() {
                        last.message = None;
                        last.error = None;
                    }
                }
            }
        }

        trim_output_to_budget(output)?;
        return Ok(false);
    }

    output.budget.used_chars = used;
    Ok(true)
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
            if !inner.items.is_empty() {
                inner.items.pop();
                return true;
            }
            false
        },
    )?;
    Ok(())
}
