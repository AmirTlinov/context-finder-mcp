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
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

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
    /// Relevance score
    pub score: f32,
    /// Code content
    pub content: String,
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
