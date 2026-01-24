use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContextRequest {
    /// Search query
    #[schemars(description = "Natural language search query")]
    pub query: String,

    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_ROOT/CONTEXT_PROJECT_ROOT (legacy: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT); non-daemon fallback: cwd). DX: when a session root is already set and no path filters are provided, a relative `path` may be treated as an in-project scope hint instead of switching roots."
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

    /// Maximum primary results (default: 5)
    #[schemars(description = "Maximum number of primary results")]
    pub limit: Option<usize>,

    /// Search strategy: direct, extended, deep
    #[schemars(
        description = "Graph traversal depth: direct (none), extended (1-hop), deep (2-hop)"
    )]
    pub strategy: Option<String>,

    /// Graph language: rust, python, javascript, typescript
    #[schemars(description = "Programming language for graph analysis")]
    pub language: Option<String>,

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
pub struct ContextResult {
    /// Primary search results
    pub results: Vec<ContextHit>,
    /// Total related code found
    pub related_count: usize,
    #[serde(default)]
    pub meta: ToolMeta,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ContextHit {
    /// File path
    pub file: String,
    /// Start line
    pub start_line: usize,
    /// End line
    pub end_line: usize,
    /// Symbol name
    pub symbol: Option<String>,
    /// Symbol type (function, struct, etc.)
    pub chunk_type: Option<String>,
    /// Fully-qualified name (module.Class.method)
    pub qualified_name: Option<String>,
    /// Parent scope (class/module for methods/functions)
    pub parent_scope: Option<String>,
    /// Relevance score
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
    /// Related code through graph
    pub related: Vec<RelatedCode>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RelatedCode {
    /// File path
    pub file: String,
    /// Line range as string
    pub lines: String,
    /// Symbol name
    pub symbol: Option<String>,
    /// Relationship path (e.g., "Calls", "Uses -> Uses")
    pub relationship: String,
}
