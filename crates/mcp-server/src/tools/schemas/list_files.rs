use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListFilesRequest {
    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

    /// Optional file path filter (simple glob: '*' and '?' supported). If no glob metachars are
    /// present, treated as substring match against the relative file path.
    #[schemars(description = "Optional file path filter (glob or substring)")]
    pub file_pattern: Option<String>,

    /// Maximum number of files to return (default: 200)
    #[schemars(description = "Maximum number of file paths to return (bounded)")]
    pub limit: Option<usize>,

    /// Maximum number of UTF-8 characters across returned file paths (default: 20000)
    #[schemars(description = "Maximum number of UTF-8 characters across returned file paths")]
    pub max_chars: Option<usize>,

    /// Opaque cursor token to continue a previous response
    #[schemars(description = "Opaque cursor token to continue a previous list_files response")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::tools) struct ListFilesCursorV1 {
    pub(in crate::tools) v: u32,
    pub(in crate::tools) tool: String,
    pub(in crate::tools) root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) file_pattern: Option<String>,
    pub(in crate::tools) last_file: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ListFilesTruncation {
    Limit,
    MaxChars,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListFilesResult {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_pattern: Option<String>,
    pub scanned_files: usize,
    pub returned: usize,
    pub used_chars: usize,
    pub limit: usize,
    pub max_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<ListFilesTruncation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
    pub files: Vec<String>,
}
