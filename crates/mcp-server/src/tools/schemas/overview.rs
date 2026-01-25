use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OverviewRequest {
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

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OverviewResult {
    /// Project info
    pub project: ProjectInfo,
    /// Architecture layers
    pub layers: Vec<LayerInfo>,
    /// Entry points
    pub entry_points: Vec<String>,
    /// Key types (most connected)
    pub key_types: Vec<KeyTypeInfo>,
    /// Graph statistics
    pub graph_stats: GraphStats,
    #[serde(default)]
    pub meta: ToolMeta,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectInfo {
    pub name: String,
    pub files: usize,
    pub chunks: usize,
    pub lines: usize,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LayerInfo {
    pub name: String,
    pub files: usize,
    pub role: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct KeyTypeInfo {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub coupling: usize,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GraphStats {
    pub nodes: usize,
    pub edges: usize,
}
