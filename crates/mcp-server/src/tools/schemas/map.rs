use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MapRequest {
    /// Project directory path (defaults to session root; fallback: env/git/cwd)
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

    /// Directory depth for aggregation (default: 2)
    #[schemars(description = "Directory depth for grouping (1-4)")]
    pub depth: Option<usize>,

    /// Maximum number of directories to return
    #[schemars(description = "Limit number of results")]
    pub limit: Option<usize>,

    /// Opaque cursor token to continue a previous response
    #[schemars(description = "Opaque cursor token to continue a previous map response")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::tools) struct MapCursorV1 {
    pub(in crate::tools) v: u32,
    pub(in crate::tools) tool: String,
    pub(in crate::tools) root: String,
    pub(in crate::tools) depth: usize,
    pub(in crate::tools) offset: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MapResult {
    /// Total files in project
    pub total_files: usize,
    /// Total code chunks indexed
    pub total_chunks: usize,
    /// Total lines of code
    pub total_lines: usize,
    /// Directory breakdown
    pub directories: Vec<DirectoryInfo>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct DirectoryInfo {
    /// Directory path
    pub path: String,
    /// Number of files
    pub files: usize,
    /// Number of chunks
    pub chunks: usize,
    /// Percentage of codebase
    pub coverage_pct: f32,
    /// Top symbols in this directory
    pub top_symbols: Vec<String>,
}
