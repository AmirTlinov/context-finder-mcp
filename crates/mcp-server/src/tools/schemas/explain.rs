use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExplainRequest {
    /// Symbol name to explain
    #[schemars(description = "Symbol name to get detailed information about")]
    pub symbol: String,

    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_ROOT/CONTEXT_PROJECT_ROOT; non-daemon fallback: cwd)."
    )]
    pub path: Option<String>,

    /// Programming language
    #[schemars(description = "Programming language: rust, python, javascript, typescript")]
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
pub struct ExplainResult {
    /// Symbol name
    pub symbol: String,
    /// Symbol kind (function, struct, etc.)
    pub kind: String,
    /// File path
    pub file: String,
    /// Line number
    pub line: usize,
    /// Documentation (if available)
    pub documentation: Option<String>,
    /// Dependencies (what this symbol uses/calls)
    pub dependencies: Vec<String>,
    /// Dependents (what uses/calls this symbol)
    pub dependents: Vec<String>,
    /// Related tests
    pub tests: Vec<String>,
    /// Code content
    pub content: String,
    #[serde(default)]
    pub meta: ToolMeta,
}
