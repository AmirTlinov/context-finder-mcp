use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ImpactRequest {
    /// Symbol name to analyze
    #[schemars(description = "Symbol name to find usages of (e.g., 'VectorStore', 'search')")]
    pub symbol: String,

    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

    /// Depth of transitive usages (1=direct, 2=transitive)
    #[schemars(description = "Depth for transitive impact analysis (1-3)")]
    pub depth: Option<usize>,

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
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
