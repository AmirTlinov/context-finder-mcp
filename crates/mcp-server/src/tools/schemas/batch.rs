use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatchToolName {
    Map,
    FileSlice,
    ListFiles,
    TextSearch,
    GrepContext,
    Doctor,
    Search,
    Context,
    ContextPack,
    Index,
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

    /// If true, stop processing after the first item error.
    #[schemars(description = "If true, stop processing after the first item error.")]
    #[serde(default)]
    pub stop_on_error: bool,

    /// Batch items to execute.
    #[schemars(description = "Batch items to execute.")]
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
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct BatchItemResult {
    pub id: String,
    pub tool: BatchToolName,
    pub status: BatchItemStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub data: serde_json::Value,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct BatchResult {
    pub version: u32,
    pub items: Vec<BatchItemResult>,
    pub budget: BatchBudget,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
}
