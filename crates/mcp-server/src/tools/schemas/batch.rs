use anyhow::Context as _;
use context_indexer::ToolMeta;
use context_protocol::{BudgetTruncation, ErrorEnvelope, ToolNextAction};
use rmcp::schemars;
use serde::de::Error as _;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatchToolName {
    Capabilities,
    Help,
    Map,
    FileSlice,
    ListFiles,
    TextSearch,
    GrepContext,
    Doctor,
    Search,
    Context,
    ContextPack,
    NotebookPack,
    NotebookSuggest,
    RunbookPack,
    MeaningPack,
    MeaningFocus,
    WorktreePack,
    AtlasPack,
    EvidenceFetch,
    Impact,
    Trace,
    Explain,
    Overview,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchRequest {
    /// Batch schema version (default: 2).
    ///
    /// - v1: executes items sequentially, but does NOT resolve `$ref` wrappers.
    /// - v2: resolves `$ref` wrappers (id-based JSON Pointer) against prior item results.
    ///
    /// Note: Batch v2 `$ref` semantics are shared with Command API batch v1 via `crates/batch-ref`.
    #[schemars(
        description = "Batch schema version (default: 2). v1: no $ref resolution. v2: supports $ref wrappers (id-based JSON Pointer) against prior item results."
    )]
    pub version: Option<u32>,

    /// Project directory path (defaults to session root; fallback: env/git/cwd)
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd). Alias: `project`."
    )]
    #[serde(alias = "project")]
    pub path: Option<String>,

    /// Maximum number of UTF-8 characters for the serialized batch result (best effort).
    #[schemars(
        description = "Maximum number of UTF-8 characters for the serialized batch result (best effort)."
    )]
    pub max_chars: Option<usize>,

    /// Response mode:
    /// - "facts" (default): keeps meta/index_state for freshness, strips next_actions to reduce noise.
    /// - "full": includes meta/index_state and next_actions (when applicable).
    /// - "minimal": strips meta/index_state and next_actions to reduce noise.
    #[schemars(description = "Response mode: 'facts' (default), 'full', or 'minimal'")]
    pub response_mode: Option<ResponseMode>,

    /// If true, stop processing after the first item error.
    #[schemars(description = "If true, stop processing after the first item error.")]
    #[serde(default)]
    pub stop_on_error: bool,

    /// Batch items to execute.
    #[schemars(description = "Batch items to execute.")]
    #[serde(deserialize_with = "deserialize_batch_items")]
    pub items: Vec<BatchItem>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchItem {
    /// Caller-provided identifier used to correlate results (trimmed).
    ///
    /// Must be non-empty. In batch v2, ids must be unique within the batch and are used for `$ref`
    /// pointers (`#/items/<id>/data/...`) into prior item results.
    pub id: String,

    /// Tool name to execute (alias: action).
    #[serde(alias = "action")]
    pub tool: BatchToolName,

    /// Tool input object (tool-specific). Defaults to `{}`.
    ///
    /// In batch v2, any value position may be a `$ref` wrapper:
    /// `{ "$ref": "#/items/<id>/data/...", "$default": <optional> }`.
    /// The wrapper is recognized only when the object contains exactly `$ref` (+ optional `$default`).
    #[serde(default, alias = "payload")]
    pub input: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BatchItemWire {
    Object(BatchItem),
    String(String),
}

fn deserialize_batch_items<'de, D>(deserializer: D) -> Result<Vec<BatchItem>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let items: Vec<BatchItemWire> = Vec::deserialize(deserializer)?;
    items
        .into_iter()
        .enumerate()
        .map(|(idx, item)| match item {
            BatchItemWire::Object(item) => Ok(item),
            BatchItemWire::String(raw) => {
                parse_legacy_batch_item(idx, &raw).map_err(D::Error::custom)
            }
        })
        .collect()
}

fn parse_legacy_batch_item(index: usize, raw: &str) -> anyhow::Result<BatchItem> {
    let text = raw.trim();
    if text.is_empty() {
        anyhow::bail!("batch item string must be non-empty");
    }

    // Allow embedding a full BatchItem JSON object as a string.
    if text.starts_with('{') {
        let value: serde_json::Value = serde_json::from_str(text)
            .with_context(|| format!("parse batch item #{index} as JSON"))?;
        let item: BatchItem = serde_json::from_value(value)
            .with_context(|| format!("decode batch item #{index} JSON into BatchItem"))?;
        return Ok(item);
    }

    // Legacy DSL: "<tool> <json?>"
    // Example: "meaning_pack {\"query\":\"...\"}"
    let mut parts = text.splitn(2, char::is_whitespace);
    let tool_raw = parts.next().unwrap_or("").trim();
    if tool_raw.is_empty() {
        anyhow::bail!("batch item #{index}: missing tool name");
    }
    let tool: BatchToolName =
        serde_json::from_value(serde_json::Value::String(tool_raw.to_string()))
            .with_context(|| format!("batch item #{index}: invalid tool '{tool_raw}'"))?;

    let rest = parts.next().unwrap_or("").trim();
    let input = if rest.is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(rest).with_context(|| {
            format!("batch item #{index}: parse input JSON for tool '{tool_raw}'")
        })?
    };

    Ok(BatchItem {
        id: format!("legacy_{index}_{tool_raw}"),
        tool,
        input,
    })
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatchItemStatus {
    Ok,
    Error,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct BatchBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<BudgetTruncation>,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct BatchItemResult {
    pub id: String,
    pub tool: BatchToolName,
    pub status: BatchItemStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorEnvelope>,
    pub data: serde_json::Value,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct BatchResult {
    pub version: u32,
    pub items: Vec<BatchItemResult>,
    pub budget: BatchBudget,
    #[serde(default)]
    pub next_actions: Vec<ToolNextAction>,
    pub meta: ToolMeta,
}
