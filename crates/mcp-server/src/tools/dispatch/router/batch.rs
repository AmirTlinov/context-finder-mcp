use super::super::{
    compute_used_chars, extract_path_from_input, parse_tool_result_as_json, prepare_item_input,
    push_item_or_truncate, resolve_batch_refs, BatchBudget, BatchItemResult, BatchItemStatus,
    BatchRequest, BatchResult, BatchToolName, CallToolResult, Content, ContextFinderService,
    ContextPackRequest, ContextRequest, DoctorRequest, ExplainRequest, FileSliceRequest,
    GrepContextRequest, ImpactRequest, IndexRequest, ListFilesRequest, MapRequest, McpError,
    OverviewRequest, Parameters, SearchRequest, TextSearchRequest, TraceRequest,
};
use crate::tools::schemas::batch::BatchItem;
use std::collections::HashSet;
use std::path::PathBuf;

const DEFAULT_MAX_CHARS: usize = 20_000;
const MAX_MAX_CHARS: usize = 500_000;
const MIN_SUPPORTED_VERSION: u32 = 1;
const LATEST_VERSION: u32 = 2;
const DEFAULT_VERSION: u32 = LATEST_VERSION;

fn call_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.into())])
}

fn validate_batch_version(version: u32) -> Option<String> {
    if (MIN_SUPPORTED_VERSION..=LATEST_VERSION).contains(&version) {
        None
    } else {
        Some(format!(
            "Unsupported batch version {version} (supported: {MIN_SUPPORTED_VERSION}..={LATEST_VERSION})"
        ))
    }
}

async fn dispatch_tool(
    service: &ContextFinderService,
    tool: BatchToolName,
    input: serde_json::Value,
) -> std::result::Result<CallToolResult, McpError> {
    macro_rules! typed_call {
        ($req:ty, $method:ident, $tool_name:literal) => {{
            match serde_json::from_value::<$req>(input) {
                Ok(req) => service.$method(Parameters(req)).await,
                Err(err) => Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid input for {}: {err}",
                    $tool_name
                ))])),
            }
        }};
    }

    match tool {
        BatchToolName::Map => typed_call!(MapRequest, map, "map"),
        BatchToolName::FileSlice => typed_call!(FileSliceRequest, file_slice, "file_slice"),
        BatchToolName::ListFiles => typed_call!(ListFilesRequest, list_files, "list_files"),
        BatchToolName::TextSearch => typed_call!(TextSearchRequest, text_search, "text_search"),
        BatchToolName::GrepContext => typed_call!(GrepContextRequest, grep_context, "grep_context"),
        BatchToolName::Doctor => typed_call!(DoctorRequest, doctor, "doctor"),
        BatchToolName::Search => typed_call!(SearchRequest, search, "search"),
        BatchToolName::Context => typed_call!(ContextRequest, context, "context"),
        BatchToolName::ContextPack => typed_call!(ContextPackRequest, context_pack, "context_pack"),
        BatchToolName::Index => typed_call!(IndexRequest, index, "index"),
        BatchToolName::Impact => typed_call!(ImpactRequest, impact, "impact"),
        BatchToolName::Trace => typed_call!(TraceRequest, trace, "trace"),
        BatchToolName::Explain => typed_call!(ExplainRequest, explain, "explain"),
        BatchToolName::Overview => typed_call!(OverviewRequest, overview, "overview"),
    }
}

struct BatchRunner<'a> {
    service: &'a ContextFinderService,
    stop_on_error: bool,
    inferred_path: Option<String>,
    seen_ids: HashSet<String>,
    ref_context: Option<serde_json::Value>,
    output: BatchResult,
}

impl<'a> BatchRunner<'a> {
    fn new(
        service: &'a ContextFinderService,
        version: u32,
        max_chars: usize,
        inferred_path: Option<String>,
    ) -> Self {
        let output = BatchResult {
            version,
            items: Vec::new(),
            budget: BatchBudget {
                max_chars,
                used_chars: 0,
                truncated: false,
            },
            meta: None,
        };
        let ref_context = (version >= 2).then(|| {
            serde_json::json!({
                "project": inferred_path.clone(),
                "path": inferred_path.clone(),
                "items": serde_json::Value::Object(serde_json::Map::new()),
            })
        });

        Self {
            service,
            stop_on_error: false,
            inferred_path,
            seen_ids: HashSet::new(),
            ref_context,
            output,
        }
    }

    const fn with_stop_on_error(mut self, stop_on_error: bool) -> Self {
        self.stop_on_error = stop_on_error;
        self
    }

    const fn remaining_chars(&self) -> usize {
        self.output
            .budget
            .max_chars
            .saturating_sub(self.output.budget.used_chars)
    }

    fn update_ref_context_path(&mut self) {
        let Some(ctx) = self.ref_context.as_mut() else {
            return;
        };

        let value = self
            .inferred_path
            .as_ref()
            .map_or(serde_json::Value::Null, |value| {
                serde_json::Value::String(value.clone())
            });
        ctx["project"] = value.clone();
        ctx["path"] = value;
    }

    fn store_last_item_in_ref_context(&mut self) {
        let Some(ctx) = self.ref_context.as_mut() else {
            return;
        };
        let Some(items) = ctx
            .get_mut("items")
            .and_then(serde_json::Value::as_object_mut)
        else {
            return;
        };
        let Some(stored) = self.output.items.last() else {
            return;
        };

        items.insert(
            stored.id.clone(),
            serde_json::json!({
                "tool": stored.tool,
                "status": stored.status,
                "message": stored.message,
                "data": stored.data,
            }),
        );
    }

    fn push_rejected(&mut self, id: String, tool: BatchToolName, message: String) -> bool {
        let rejected = BatchItemResult {
            id,
            tool,
            status: BatchItemStatus::Error,
            message: Some(message),
            data: serde_json::Value::Null,
        };

        if !push_item_or_truncate(&mut self.output, rejected) {
            return false;
        }
        self.store_last_item_in_ref_context();

        !self.stop_on_error
    }

    fn push_processed(&mut self, item: BatchItemResult) -> bool {
        if !push_item_or_truncate(&mut self.output, item) {
            return false;
        }
        self.store_last_item_in_ref_context();
        !(self.stop_on_error
            && self
                .output
                .items
                .last()
                .is_some_and(|v| v.status == BatchItemStatus::Error))
    }

    async fn run_item(&mut self, item: BatchItem) -> bool {
        let trimmed_id = item.id.trim().to_string();
        if trimmed_id.is_empty() {
            return self.push_rejected(
                item.id,
                item.tool,
                "Batch item id must not be empty".to_string(),
            );
        }

        if !self.seen_ids.insert(trimmed_id.clone()) {
            let message = format!("Duplicate batch item id is not supported: '{trimmed_id}'");
            return self.push_rejected(trimmed_id, item.tool, message);
        }

        let resolved_input = if let Some(ctx) = self.ref_context.as_ref() {
            match resolve_batch_refs(item.input, ctx) {
                Ok(value) => value,
                Err(err) => {
                    return self.push_rejected(
                        trimmed_id,
                        item.tool,
                        format!("Ref resolution error: {err}"),
                    );
                }
            }
        } else {
            item.input
        };

        if let Some(item_path) = extract_path_from_input(&resolved_input) {
            if let Some(batch_path) = self.inferred_path.as_deref() {
                if batch_path != item_path {
                    return self.push_rejected(
                        trimmed_id,
                        item.tool,
                        format!(
                            "Batch path mismatch: batch uses '{batch_path}', item uses '{item_path}'"
                        ),
                    );
                }
            } else {
                self.inferred_path = Some(item_path);
                self.update_ref_context_path();
            }
        }

        let input = prepare_item_input(
            resolved_input,
            self.inferred_path.as_deref(),
            item.tool,
            self.remaining_chars(),
        );
        let tool_result = dispatch_tool(self.service, item.tool, input).await;
        let outcome = materialize_item_result(trimmed_id, item.tool, tool_result);

        self.push_processed(outcome)
    }

    fn finish(self) -> CallToolResult {
        CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&self.output).unwrap_or_default(),
        )])
    }

    async fn apply_meta(&mut self) {
        let Some(raw_path) = self.inferred_path.as_deref() else {
            return;
        };
        let Ok(root) = PathBuf::from(raw_path).canonicalize() else {
            return;
        };
        self.output.meta = Some(self.service.tool_meta(&root).await);
        if let Ok(used_chars) = compute_used_chars(&self.output) {
            self.output.budget.used_chars = used_chars;
            if used_chars > self.output.budget.max_chars {
                self.output.budget.truncated = true;
            }
        }
    }
}

fn materialize_item_result(
    id: String,
    tool: BatchToolName,
    tool_result: std::result::Result<CallToolResult, McpError>,
) -> BatchItemResult {
    match tool_result {
        Ok(result) => match parse_tool_result_as_json(&result, tool) {
            Ok(data) => BatchItemResult {
                id,
                tool,
                status: BatchItemStatus::Ok,
                message: None,
                data,
            },
            Err(message) => BatchItemResult {
                id,
                tool,
                status: BatchItemStatus::Error,
                message: Some(message),
                data: serde_json::Value::Null,
            },
        },
        Err(err) => BatchItemResult {
            id,
            tool,
            status: BatchItemStatus::Error,
            message: Some(err.to_string()),
            data: serde_json::Value::Null,
        },
    }
}

/// Execute multiple Context Finder tools in a single call (agent-friendly batch).
pub(in crate::tools::dispatch) async fn batch(
    service: &ContextFinderService,
    request: BatchRequest,
) -> Result<CallToolResult, McpError> {
    if request.items.is_empty() {
        return Ok(call_error("Batch items must not be empty"));
    }

    let max_chars = request
        .max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(1, MAX_MAX_CHARS);

    let version = request.version.unwrap_or(DEFAULT_VERSION);
    if let Some(message) = validate_batch_version(version) {
        return Ok(call_error(message));
    }

    let inferred_path = match service.resolve_root(request.path.as_deref()).await {
        Ok((_, root_display)) => Some(root_display),
        Err(message) => return Ok(call_error(message)),
    };
    let mut runner = BatchRunner::new(service, version, max_chars, inferred_path)
        .with_stop_on_error(request.stop_on_error);
    runner.update_ref_context_path();

    for item in request.items {
        if !runner.run_item(item).await {
            break;
        }
    }

    runner.apply_meta().await;
    Ok(runner.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_versions_are_stable() {
        assert_eq!(MIN_SUPPORTED_VERSION, 1);
        assert_eq!(LATEST_VERSION, 2);
        assert_eq!(DEFAULT_VERSION, LATEST_VERSION);
    }

    #[test]
    fn validate_batch_version_rejects_out_of_range() {
        assert!(validate_batch_version(1).is_none());
        assert!(validate_batch_version(2).is_none());
        assert!(validate_batch_version(0).is_some());
        assert!(validate_batch_version(3).is_some());
    }
}
