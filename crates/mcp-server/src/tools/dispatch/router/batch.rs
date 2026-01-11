use super::super::{
    compute_used_chars, extract_path_from_input, parse_tool_result_as_json, prepare_item_input,
    push_item_or_truncate, resolve_batch_refs, trim_output_to_budget, BatchBudget, BatchItemResult,
    BatchItemStatus, BatchRequest, BatchResult, BatchToolName, CallToolResult, CapabilitiesRequest,
    Content, ContextFinderService, ContextPackRequest, ContextRequest, DoctorRequest,
    EvidenceFetchRequest, ExplainRequest, FileSliceRequest, GrepContextRequest, HelpRequest,
    ImpactRequest, ListFilesRequest, MapRequest, McpError, MeaningFocusRequest, MeaningPackRequest,
    OverviewRequest, ResponseMode, SearchRequest, TextSearchRequest, ToolMeta, TraceRequest,
};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::schemas::batch::BatchItem;
use context_protocol::ErrorEnvelope;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::error::{
    attach_meta, attach_structured_content, invalid_request, invalid_request_with,
    invalid_request_with_meta, meta_for_request,
};
const DEFAULT_MAX_CHARS: usize = 2_000;
const MAX_MAX_CHARS: usize = 500_000;
const MIN_SUPPORTED_VERSION: u32 = 1;
const LATEST_VERSION: u32 = 2;
const DEFAULT_VERSION: u32 = LATEST_VERSION;

type ToolResult<T> = std::result::Result<T, CallToolResult>;

fn suggest_max_chars(current: usize) -> usize {
    current
        .saturating_mul(2)
        .clamp(DEFAULT_MAX_CHARS, MAX_MAX_CHARS)
}

fn retry_action(
    max_chars: usize,
    path: Option<&str>,
    version: u32,
) -> context_protocol::ToolNextAction {
    let mut args = serde_json::json!({
        "version": version,
        "max_chars": max_chars,
    });
    if let Some(path) = path {
        args["path"] = serde_json::Value::String(path.to_string());
    }
    context_protocol::ToolNextAction {
        tool: "batch".to_string(),
        args,
        reason: "Increase max_chars to fit the batch response envelope.".to_string(),
    }
}

fn budget_error(
    max_chars: usize,
    path: Option<&str>,
    version: u32,
    err: anyhow::Error,
) -> CallToolResult {
    let suggested = suggest_max_chars(max_chars);
    // Even in low-noise modes, budget errors should return a deterministic recovery action.
    // This keeps the tool "self-healing" for agents.
    let next_actions = vec![retry_action(suggested, path, version)];
    invalid_request_with(
        format!("max_chars too small for batch response ({err:#})"),
        Some(format!("Increase max_chars (suggested: {suggested}).")),
        next_actions,
    )
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

fn batch_tool_name_label(tool: BatchToolName) -> &'static str {
    match tool {
        BatchToolName::Capabilities => "capabilities",
        BatchToolName::Help => "help",
        BatchToolName::Map => "map",
        BatchToolName::FileSlice => "file_slice",
        BatchToolName::ListFiles => "list_files",
        BatchToolName::TextSearch => "text_search",
        BatchToolName::GrepContext => "grep_context",
        BatchToolName::Doctor => "doctor",
        BatchToolName::Search => "search",
        BatchToolName::Context => "context",
        BatchToolName::ContextPack => "context_pack",
        BatchToolName::MeaningPack => "meaning_pack",
        BatchToolName::MeaningFocus => "meaning_focus",
        BatchToolName::EvidenceFetch => "evidence_fetch",
        BatchToolName::Impact => "impact",
        BatchToolName::Trace => "trace",
        BatchToolName::Explain => "explain",
        BatchToolName::Overview => "overview",
    }
}

fn batch_item_status_label(status: BatchItemStatus) -> &'static str {
    match status {
        BatchItemStatus::Ok => "ok",
        BatchItemStatus::Error => "error",
    }
}

fn render_batch_context_doc(
    output: &BatchResult,
    docs_by_id: &HashMap<String, String>,
    _response_mode: ResponseMode,
) -> String {
    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!(
        "batch: items={} truncated={}",
        output.items.len(),
        output.budget.truncated
    ));
    doc.push_root_fingerprint(output.meta.root_fingerprint);

    for item in &output.items {
        doc.push_note(&format!(
            "item {}: tool={} status={}",
            item.id,
            batch_tool_name_label(item.tool),
            batch_item_status_label(item.status)
        ));

        if let Some(text) = docs_by_id.get(&item.id) {
            doc.push_block_smart(text);
        } else if let Some(message) = item.message.as_deref() {
            doc.push_block_smart(message);
        }
        doc.push_blank();
    }

    if output.budget.truncated {
        doc.push_note("truncated=true (increase max_chars)");
    }

    doc.finish()
}

async fn dispatch_tool(
    service: &ContextFinderService,
    tool: BatchToolName,
    input: serde_json::Value,
) -> std::result::Result<CallToolResult, McpError> {
    macro_rules! typed_call {
        ($req:ty, $func:path, $tool_name:literal) => {{
            match serde_json::from_value::<$req>(input) {
                Ok(req) => $func(service, req).await,
                Err(err) => Ok(invalid_request(format!(
                    "Invalid input for {}: {err}",
                    $tool_name
                ))),
            }
        }};
    }

    match tool {
        BatchToolName::Capabilities => typed_call!(
            CapabilitiesRequest,
            super::capabilities::capabilities,
            "capabilities"
        ),
        BatchToolName::Help => typed_call!(HelpRequest, super::help::help, "help"),
        BatchToolName::Map => typed_call!(MapRequest, super::map::map, "map"),
        BatchToolName::FileSlice => match serde_json::from_value::<FileSliceRequest>(input) {
            Ok(req) => super::file_slice::file_slice(service, &req).await,
            Err(err) => Ok(invalid_request(format!(
                "Invalid input for file_slice: {err}"
            ))),
        },
        BatchToolName::ListFiles => {
            typed_call!(
                ListFilesRequest,
                super::list_files::list_files,
                "list_files"
            )
        }
        BatchToolName::TextSearch => {
            typed_call!(
                TextSearchRequest,
                super::text_search::text_search,
                "text_search"
            )
        }
        BatchToolName::GrepContext => typed_call!(
            GrepContextRequest,
            super::grep_context::grep_context,
            "grep_context"
        ),
        BatchToolName::Doctor => typed_call!(DoctorRequest, super::doctor::doctor, "doctor"),
        BatchToolName::Search => typed_call!(SearchRequest, super::search::search, "search"),
        BatchToolName::Context => typed_call!(ContextRequest, super::context::context, "context"),
        BatchToolName::ContextPack => typed_call!(
            ContextPackRequest,
            super::context_pack::context_pack,
            "context_pack"
        ),
        BatchToolName::MeaningPack => typed_call!(
            MeaningPackRequest,
            super::meaning_pack::meaning_pack,
            "meaning_pack"
        ),
        BatchToolName::MeaningFocus => typed_call!(
            MeaningFocusRequest,
            super::meaning_focus::meaning_focus,
            "meaning_focus"
        ),
        BatchToolName::EvidenceFetch => typed_call!(
            EvidenceFetchRequest,
            super::evidence_fetch::evidence_fetch,
            "evidence_fetch"
        ),
        BatchToolName::Impact => typed_call!(ImpactRequest, super::impact::impact, "impact"),
        BatchToolName::Trace => typed_call!(TraceRequest, super::trace::trace, "trace"),
        BatchToolName::Explain => typed_call!(ExplainRequest, super::explain::explain, "explain"),
        BatchToolName::Overview => {
            typed_call!(OverviewRequest, super::overview::overview, "overview")
        }
    }
}

struct BatchRunner<'a> {
    service: &'a ContextFinderService,
    stop_on_error: bool,
    inferred_path: Option<String>,
    seen_ids: HashSet<String>,
    ref_context: Option<serde_json::Value>,
    output: BatchResult,
    response_mode: ResponseMode,
    docs_by_id: HashMap<String, String>,
}

impl<'a> BatchRunner<'a> {
    fn new(
        service: &'a ContextFinderService,
        version: u32,
        max_chars: usize,
        inferred_path: Option<String>,
        response_mode: ResponseMode,
    ) -> Self {
        let output = BatchResult {
            version,
            items: Vec::new(),
            budget: BatchBudget {
                max_chars,
                used_chars: 0,
                truncated: false,
                truncation: None,
            },
            next_actions: Vec::new(),
            meta: ToolMeta::default(),
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
            response_mode,
            docs_by_id: HashMap::new(),
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

    fn push_rejected(
        &mut self,
        id: String,
        tool: BatchToolName,
        message: String,
    ) -> ToolResult<bool> {
        let rejected = batch_error_item(id, tool, "invalid_request", message);

        let pushed = push_item_or_truncate(&mut self.output, rejected).map_err(|err| {
            budget_error(
                self.output.budget.max_chars,
                self.inferred_path.as_deref(),
                self.output.version,
                err,
            )
        })?;
        if !pushed {
            return Ok(false);
        }
        self.store_last_item_in_ref_context();

        Ok(!self.stop_on_error)
    }

    fn push_processed(&mut self, item: BatchItemResult) -> ToolResult<bool> {
        let pushed = push_item_or_truncate(&mut self.output, item).map_err(|err| {
            budget_error(
                self.output.budget.max_chars,
                self.inferred_path.as_deref(),
                self.output.version,
                err,
            )
        })?;
        if !pushed {
            return Ok(false);
        }
        self.store_last_item_in_ref_context();
        Ok(!(self.stop_on_error
            && self
                .output
                .items
                .last()
                .is_some_and(|v| v.status == BatchItemStatus::Error)))
    }

    async fn run_item(&mut self, item: BatchItem) -> ToolResult<bool> {
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
        if let Ok(ref result) = tool_result {
            let text = result
                .content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n");
            if !text.trim().is_empty() {
                self.docs_by_id.insert(trimmed_id.clone(), text);
            }
        }
        let outcome = materialize_item_result(trimmed_id, item.tool, tool_result);

        self.push_processed(outcome)
    }

    fn finish(self) -> CallToolResult {
        let doc = render_batch_context_doc(&self.output, &self.docs_by_id, self.response_mode);
        let result = CallToolResult::success(vec![Content::text(doc)]);
        attach_structured_content(result, &self.output, self.output.meta.clone(), "batch")
    }

    async fn apply_meta(&mut self) -> ToolResult<()> {
        let Some(raw_path) = self.inferred_path.as_deref() else {
            return Ok(());
        };
        let Ok(root) = PathBuf::from(raw_path).canonicalize() else {
            return Ok(());
        };
        if self.response_mode != ResponseMode::Minimal {
            self.output.meta = self.service.tool_meta(&root).await;
        }
        trim_output_to_budget(&mut self.output).map_err(|err| {
            budget_error(
                self.output.budget.max_chars,
                self.inferred_path.as_deref(),
                self.output.version,
                err,
            )
        })?;
        if self.response_mode == ResponseMode::Full
            && self.output.budget.truncated
            && self.output.next_actions.is_empty()
        {
            let suggested = suggest_max_chars(self.output.budget.max_chars);
            self.output.next_actions.push(retry_action(
                suggested,
                self.inferred_path.as_deref(),
                self.output.version,
            ));
            trim_output_to_budget(&mut self.output).map_err(|err| {
                budget_error(
                    self.output.budget.max_chars,
                    self.inferred_path.as_deref(),
                    self.output.version,
                    err,
                )
            })?;
        }

        if self.response_mode != ResponseMode::Full {
            self.output.next_actions.clear();
        }
        Ok(())
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
                error: None,
                data,
            },
            Err(message) => {
                let error = extract_error_envelope(&result).unwrap_or_else(|| ErrorEnvelope {
                    code: if result.is_error.unwrap_or(false) {
                        "tool_error".to_string()
                    } else {
                        "invalid_response".to_string()
                    },
                    message: message.clone(),
                    details: None,
                    hint: None,
                    next_actions: Vec::new(),
                });
                BatchItemResult {
                    id,
                    tool,
                    status: BatchItemStatus::Error,
                    message: Some(error.message.clone()),
                    error: Some(error),
                    data: serde_json::Value::Null,
                }
            }
        },
        Err(err) => {
            let error = ErrorEnvelope {
                code: "tool_error".to_string(),
                message: err.to_string(),
                details: None,
                hint: None,
                next_actions: Vec::new(),
            };
            BatchItemResult {
                id,
                tool,
                status: BatchItemStatus::Error,
                message: Some(error.message.clone()),
                error: Some(error),
                data: serde_json::Value::Null,
            }
        }
    }
}

fn batch_error_item(
    id: String,
    tool: BatchToolName,
    code: &str,
    message: String,
) -> BatchItemResult {
    BatchItemResult {
        id,
        tool,
        status: BatchItemStatus::Error,
        message: Some(message.clone()),
        error: Some(ErrorEnvelope {
            code: code.to_string(),
            message,
            details: None,
            hint: None,
            next_actions: Vec::new(),
        }),
        data: serde_json::Value::Null,
    }
}

fn extract_error_envelope(result: &CallToolResult) -> Option<ErrorEnvelope> {
    let content = result.structured_content.as_ref()?;
    let raw = content.get("error")?.clone();
    serde_json::from_value(raw).ok()
}

/// Execute multiple Context tools in a single call (agent-friendly batch).
pub(in crate::tools::dispatch) async fn batch(
    service: &ContextFinderService,
    request: BatchRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);

    let mut meta = if response_mode == ResponseMode::Minimal {
        ToolMeta::default()
    } else {
        meta_for_request(service, request.path.as_deref()).await
    };
    if request.items.is_empty() {
        return Ok(invalid_request_with_meta(
            "Batch items must not be empty",
            meta,
            None,
            Vec::new(),
        ));
    }

    let max_chars = request
        .max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(1, MAX_MAX_CHARS);

    let version = request.version.unwrap_or(DEFAULT_VERSION);
    if let Some(message) = validate_batch_version(version) {
        return Ok(invalid_request_with_meta(message, meta, None, Vec::new()));
    }

    let min_payload = BatchResult {
        version,
        items: Vec::new(),
        budget: BatchBudget {
            max_chars,
            used_chars: 0,
            truncated: true,
            truncation: None,
        },
        next_actions: Vec::new(),
        meta: ToolMeta::default(),
    };
    if let Ok(min_chars) = compute_used_chars(&min_payload) {
        if min_chars > max_chars {
            let suggested = suggest_max_chars(max_chars);
            return Ok(invalid_request_with_meta(
                format!("max_chars too small for batch envelope (min_chars={min_chars})"),
                meta,
                Some(format!("Increase max_chars (suggested: {suggested}).")),
                vec![retry_action(suggested, request.path.as_deref(), version)],
            ));
        }
    }

    let inferred_path = match service
        .resolve_root_no_daemon_touch(request.path.as_deref())
        .await
    {
        Ok((root, root_display)) => {
            meta = if response_mode == ResponseMode::Minimal {
                ToolMeta::default()
            } else {
                service.tool_meta(&root).await
            };
            Some(root_display)
        }
        Err(message) => {
            return Ok(invalid_request_with_meta(message, meta, None, Vec::new()));
        }
    };
    let mut runner = BatchRunner::new(service, version, max_chars, inferred_path, response_mode)
        .with_stop_on_error(request.stop_on_error);
    runner.update_ref_context_path();

    for item in request.items {
        match runner.run_item(item).await {
            Ok(true) => {}
            Ok(false) => break,
            Err(result) => return Ok(attach_meta(result, meta.clone())),
        }
    }

    if let Err(result) = runner.apply_meta().await {
        return Ok(attach_meta(result, meta));
    }
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
