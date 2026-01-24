use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ImpactRequest {
    /// Symbol name to analyze
    #[schemars(description = "Symbol name to find usages of (e.g., 'VectorStore', 'search')")]
    pub symbol: String,

    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_ROOT/CONTEXT_PROJECT_ROOT (legacy: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT); non-daemon fallback: cwd)."
    )]
    pub path: Option<String>,

    /// Depth of transitive usages (1=direct, 2=transitive)
    #[schemars(description = "Depth for transitive impact analysis (1-3)")]
    pub depth: Option<usize>,

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
pub struct ImpactResult {
    /// Symbol that was analyzed
    pub symbol: String,
    /// Definition location
    pub definition: Option<SymbolLocation>,
    /// Total usage count
    pub total_usages: usize,
    /// Number of files affected
    pub files_affected: usize,
    /// Direct usages
    pub direct: Vec<UsageInfo>,
    /// Transitive usages (if depth > 1)
    pub transitive: Vec<UsageInfo>,
    /// Related tests
    pub tests: Vec<String>,
    /// Is part of public API
    pub public_api: bool,
    /// Mermaid diagram
    pub mermaid: String,
    #[serde(default)]
    pub meta: ToolMeta,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SymbolLocation {
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct UsageInfo {
    pub file: String,
    pub line: usize,
    pub symbol: String,
    pub relationship: String,
}
