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
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

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
    /// Relevance score (0-1)
    pub score: f32,
    /// Code content
    pub content: String,
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
