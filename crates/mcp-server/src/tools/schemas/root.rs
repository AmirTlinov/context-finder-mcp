use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct RootGetRequest {}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RootGetResult {
    /// Current per-connection session root (when established).
    pub session_root: Option<String>,

    /// If the session root was established from a file hint, this is the relative file path.
    pub focus_file: Option<String>,

    /// Canonical MCP workspace roots reported by `roots/list` (when supported by the client).
    #[serde(default)]
    pub workspace_roots: Vec<String>,

    /// Whether `roots/list` is still in-flight after initialize.
    pub roots_pending: bool,

    /// Whether multiple workspace roots were detected and Context refused to guess a default.
    pub workspace_roots_ambiguous: bool,

    /// If set, the session root is outside workspace roots and calls must pass an explicit `path`.
    pub root_mismatch_error: Option<String>,

    #[serde(default)]
    pub meta: ToolMeta,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RootSetRequest {
    /// Project directory path (recommended: absolute).
    ///
    /// In multi-root workspaces, pass a path within the intended workspace root.
    #[schemars(description = "Project directory path (recommended: absolute).")]
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RootSetResult {
    pub session_root: String,
    pub focus_file: Option<String>,
    #[serde(default)]
    pub workspace_roots: Vec<String>,
    pub roots_pending: bool,
    pub workspace_roots_ambiguous: bool,
    pub root_mismatch_error: Option<String>,
    #[serde(default)]
    pub meta: ToolMeta,
}
