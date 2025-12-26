use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GrepContextRequest {
    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

    /// Regex pattern (Rust regex syntax)
    #[schemars(description = "Regex pattern to search for (Rust regex syntax)")]
    pub pattern: String,

    /// Optional single file path (relative to project root)
    #[schemars(description = "Optional single file path (relative to project root)")]
    pub file: Option<String>,

    /// Optional file path filter (simple glob: '*' and '?' supported). If no glob metachars are
    /// present, treated as substring match against the relative file path.
    #[schemars(description = "Optional file path filter (glob or substring)")]
    pub file_pattern: Option<String>,

    /// Symmetric context lines before and after each match (grep -C)
    #[schemars(description = "Symmetric context lines before and after each match")]
    pub context: Option<usize>,

    /// Number of lines before each match (grep -B)
    #[schemars(description = "Number of lines before each match")]
    pub before: Option<usize>,

    /// Number of lines after each match (grep -A)
    #[schemars(description = "Number of lines after each match")]
    pub after: Option<usize>,

    /// Maximum number of matching lines to process (bounded)
    #[schemars(description = "Maximum number of matching lines to process (bounded)")]
    pub max_matches: Option<usize>,

    /// Maximum number of hunks to return (bounded)
    #[schemars(description = "Maximum number of hunks to return (bounded)")]
    pub max_hunks: Option<usize>,

    /// Maximum number of UTF-8 characters across returned hunks (default: 20000)
    #[schemars(description = "Maximum number of UTF-8 characters across returned hunks")]
    pub max_chars: Option<usize>,

    /// Case-sensitive regex matching (default: true)
    #[schemars(description = "Whether regex matching is case-sensitive")]
    pub case_sensitive: Option<bool>,

    /// Opaque cursor token to continue a previous response
    #[schemars(description = "Opaque cursor token to continue a previous grep_context response")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::tools) struct GrepContextCursorV1 {
    pub(in crate::tools) v: u32,
    pub(in crate::tools) tool: String,
    pub(in crate::tools) root: String,
    pub(in crate::tools) pattern: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) file_pattern: Option<String>,
    pub(in crate::tools) case_sensitive: bool,
    pub(in crate::tools) before: usize,
    pub(in crate::tools) after: usize,
    pub(in crate::tools) resume_file: String,
    pub(in crate::tools) resume_line: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GrepContextTruncation {
    #[serde(rename = "max_chars")]
    Chars,
    #[serde(rename = "max_matches")]
    Matches,
    #[serde(rename = "max_hunks")]
    Hunks,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GrepContextHunk {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub match_lines: Vec<usize>,
    pub content: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GrepContextResult {
    pub pattern: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_pattern: Option<String>,
    pub case_sensitive: bool,
    pub before: usize,
    pub after: usize,
    pub scanned_files: usize,
    pub matched_files: usize,
    pub returned_matches: usize,
    pub returned_hunks: usize,
    pub used_chars: usize,
    pub max_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<GrepContextTruncation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
    pub hunks: Vec<GrepContextHunk>,
}
