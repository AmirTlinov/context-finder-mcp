use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TextSearchRequest {
    /// Text pattern to search for (literal)
    #[schemars(description = "Text pattern to search for (literal substring)")]
    pub pattern: String,

    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

    /// Optional path filter (simple glob: '*' and '?' supported). If no glob metachars are
    /// present, treated as substring match against the relative file path.
    #[schemars(description = "Optional file path filter (glob or substring)")]
    pub file_pattern: Option<String>,

    /// Maximum number of matches to return (bounded)
    #[schemars(description = "Maximum number of matches to return (bounded)")]
    pub max_results: Option<usize>,

    /// Case-sensitive search (default: true)
    #[schemars(description = "Whether search is case-sensitive")]
    pub case_sensitive: Option<bool>,

    /// Whole-word match for identifier-like patterns (default: false)
    #[schemars(description = "If true, enforce identifier-like word boundaries")]
    pub whole_word: Option<bool>,

    /// Opaque cursor token to continue a previous response
    #[schemars(description = "Opaque cursor token to continue a previous text_search response")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(in crate::tools) enum TextSearchCursorModeV1 {
    Corpus {
        file_index: usize,
        chunk_index: usize,
        line_offset: usize,
    },
    Filesystem {
        file_index: usize,
        line_offset: usize,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::tools) struct TextSearchCursorV1 {
    pub(in crate::tools) v: u32,
    pub(in crate::tools) tool: String,
    pub(in crate::tools) root: String,
    pub(in crate::tools) pattern: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) file_pattern: Option<String>,
    pub(in crate::tools) case_sensitive: bool,
    pub(in crate::tools) whole_word: bool,
    #[serde(flatten)]
    pub(in crate::tools) mode: TextSearchCursorModeV1,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TextSearchResult {
    pub pattern: String,
    pub source: String,
    pub scanned_files: usize,
    pub matched_files: usize,
    pub skipped_large_files: usize,
    pub returned: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
    pub matches: Vec<TextSearchMatch>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TextSearchMatch {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub text: String,
}
