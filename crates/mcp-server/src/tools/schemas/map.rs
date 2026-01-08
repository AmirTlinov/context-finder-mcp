use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;
use super::ToolNextAction;
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

    /// Response mode:
    /// - "minimal" (default): lowest noise; strips most diagnostics and next_actions, but keeps provenance meta (`root_fingerprint`).
    /// - "facts": payload-focused; keeps lightweight counters/structure and provenance meta (`root_fingerprint`), but strips next_actions.
    /// - "full": includes meta/diagnostics (freshness index_state) and next_actions (when applicable).
    #[schemars(description = "Response mode: 'minimal' (default), 'facts', or 'full'")]
    pub response_mode: Option<ResponseMode>,

    /// Opaque cursor token to continue a previous response
    #[schemars(description = "Opaque cursor token to continue a previous map response")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::tools) struct MapCursorV1 {
    pub(in crate::tools) v: u32,
    pub(in crate::tools) tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) root_hash: Option<u64>,
    pub(in crate::tools) depth: usize,
    /// Default page size for cursor-only continuation (0 means unspecified / legacy cursor).
    #[serde(default)]
    pub(in crate::tools) limit: usize,
    pub(in crate::tools) offset: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MapResult {
    /// Total files in project
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_files: Option<usize>,
    /// Total code chunks indexed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_chunks: Option<usize>,
    /// Total lines of code
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_lines: Option<usize>,
    /// Directory breakdown
    pub directories: Vec<DirectoryInfo>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_actions: Option<Vec<ToolNextAction>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct DirectoryInfo {
    /// Directory path
    pub path: String,
    /// Number of files
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<usize>,
    /// Number of chunks
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunks: Option<usize>,
    /// Percentage of codebase
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_pct: Option<f32>,
    /// Top symbols in this directory
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_symbols: Option<Vec<String>>,
}
