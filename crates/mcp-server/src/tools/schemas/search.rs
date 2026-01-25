use context_indexer::ToolMeta;
use context_protocol::ToolNextAction;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchRequest {
    /// Search query (semantic search)
    #[schemars(description = "Natural language search query")]
    pub query: String,

    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_ROOT/CONTEXT_PROJECT_ROOT; non-daemon fallback: cwd). DX: when a session root is already set and no path filters are provided, a relative `path` may be treated as an in-project scope hint instead of switching roots."
    )]
    pub path: Option<String>,

    /// Optional include path prefixes (relative to project root).
    #[schemars(description = "Optional include path prefixes (relative to project root).")]
    pub include_paths: Option<Vec<String>>,

    /// Optional exclude path prefixes (relative to project root).
    #[schemars(description = "Optional exclude path prefixes (relative to project root).")]
    pub exclude_paths: Option<Vec<String>>,

    /// Optional file path filter (glob or substring).
    #[schemars(description = "Optional file path filter (glob or substring).")]
    pub file_pattern: Option<String>,

    /// Maximum results (default: 10)
    #[schemars(description = "Maximum number of results (1-50)")]
    pub limit: Option<usize>,

    /// Response mode:
    /// - "facts" (default): keeps meta/index_state for freshness, strips next_actions to reduce noise.
    /// - "full": includes meta/index_state and next_actions (when applicable).
    /// - "minimal": strips index_state and next_actions, but keeps provenance meta (`root_fingerprint`).
    #[schemars(description = "Response mode: 'facts' (default), 'full', or 'minimal'")]
    pub response_mode: Option<ResponseMode>,

    /// Automatically build/refresh the semantic index when needed.
    #[schemars(
        description = "Automatically build or refresh the semantic index before executing (default: true)."
    )]
    pub auto_index: Option<bool>,

    /// Auto-index time budget in milliseconds when auto_index=true.
    #[schemars(description = "Auto-index time budget in milliseconds (default: 15000).")]
    pub auto_index_budget_ms: Option<u64>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SearchResult {
    /// File path
    pub file: String,
    /// Start line
    pub start_line: usize,
    /// End line
    pub end_line: usize,
    /// Symbol name (if any)
    pub symbol: Option<String>,
    /// Symbol type (function, struct, etc.)
    pub symbol_type: Option<String>,
    /// Fully-qualified name (module.Class.method)
    pub qualified_name: Option<String>,
    /// Parent scope (class/module for methods/functions)
    pub parent_scope: Option<String>,
    /// Relevance score (0-1)
    pub score: f32,
    /// Code content
    pub content: String,
    /// Documentation/docstring (trimmed)
    pub documentation: Option<String>,
    /// Contextual imports relevant to this chunk
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_imports: Vec<String>,
    /// Tags for categorization (async, public, deprecated, etc.)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Tier/bundle markers (file/document/test)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bundle_tags: Vec<String>,
    /// Related relative paths (tests, configs, docs)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_paths: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SearchResponse {
    /// Search results (semantic hits)
    pub results: Vec<SearchResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_actions: Vec<ToolNextAction>,
    #[serde(default)]
    pub meta: ToolMeta,
}
