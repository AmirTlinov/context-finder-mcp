use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OverviewRequest {
    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

    /// Programming language
    #[schemars(description = "Programming language: rust, python, javascript, typescript")]
    pub language: Option<String>,

    /// Automatically build or refresh the semantic index before executing (default: true)
    #[schemars(
        description = "Automatically build or refresh the semantic index before executing (default: true)."
    )]
    pub auto_index: Option<bool>,

    /// Auto-index time budget in milliseconds (default: 3000)
    #[schemars(description = "Auto-index time budget in milliseconds (default: 3000).")]
    pub auto_index_budget_ms: Option<u64>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ProjectInfo {
    pub name: String,
    pub files: usize,
    pub chunks: usize,
    pub lines: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LayerInfo {
    pub name: String,
    pub files: usize,
    pub role: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct KeyTypeInfo {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub coupling: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GraphStats {
    pub nodes: usize,
    pub edges: usize,
}
