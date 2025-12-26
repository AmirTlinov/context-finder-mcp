use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FileSliceRequest {
    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

    /// File path (relative to project root)
    #[schemars(description = "File path (relative to project root)")]
    pub file: String,

    /// First line to include (1-based, default: 1)
    #[schemars(description = "First line to include (1-based)")]
    pub start_line: Option<usize>,

    /// Maximum number of lines to return (default: 200)
    #[schemars(description = "Maximum number of lines to return (bounded)")]
    pub max_lines: Option<usize>,

    /// Maximum number of UTF-8 characters for the returned slice (default: 20000)
    #[schemars(description = "Maximum number of UTF-8 characters for the returned slice")]
    pub max_chars: Option<usize>,

    /// Opaque cursor token to continue a previous response. When provided, `start_line` is ignored.
    #[schemars(description = "Opaque cursor token to continue a previous file_slice response")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::tools) struct FileSliceCursorV1 {
    pub(in crate::tools) v: u32,
    pub(in crate::tools) tool: String,
    pub(in crate::tools) root: String,
    pub(in crate::tools) file: String,
    pub(in crate::tools) max_lines: usize,
    pub(in crate::tools) max_chars: usize,
    pub(in crate::tools) next_start_line: usize,
    pub(in crate::tools) next_byte_offset: u64,
    pub(in crate::tools) file_size_bytes: u64,
    pub(in crate::tools) file_mtime_ms: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileSliceTruncation {
    MaxLines,
    MaxChars,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FileSliceResult {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub returned_lines: usize,
    pub used_chars: usize,
    pub max_lines: usize,
    pub max_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<FileSliceTruncation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
    pub file_size_bytes: u64,
    pub file_mtime_ms: u64,
    pub content_sha256: String,
    pub content: String,
}
