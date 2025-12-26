use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExplainRequest {
    /// Symbol name to explain
    #[schemars(description = "Symbol name to get detailed information about")]
    pub symbol: String,

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
}
