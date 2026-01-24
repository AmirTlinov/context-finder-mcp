use context_indexer::ToolMeta;
use context_protocol::BudgetTruncation;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;
use super::ToolNextAction;
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TextSearchRequest {
    /// Text pattern to search for (literal).
    ///
    /// Optional when continuing with `cursor` (cursor-only continuation).
    #[schemars(
        description = "Text pattern to search for (literal substring). Optional when continuing with `cursor`."
    )]
    pub pattern: Option<String>,

    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_ROOT/CONTEXT_PROJECT_ROOT (legacy: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT); non-daemon fallback: cwd). DX: when a session root is already set and `file_pattern` is omitted, a relative `path` is treated as `file_pattern`."
    )]
    pub path: Option<String>,

    /// Hard `max_chars` budget for the `.context` response (including envelope).
    ///
    /// Under extremely small budgets the tool may truncate match text aggressively (potentially to
    /// zero matches), but it avoids failing solely due to `max_chars`.
    #[schemars(
        description = "Hard max_chars budget for the .context response (including envelope)."
    )]
    pub max_chars: Option<usize>,

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

    /// Response mode:
    /// - "minimal" (default): lowest noise; strips most diagnostics and next_actions, but keeps provenance meta (`root_fingerprint`).
    /// - "facts": payload-focused output; keeps provenance meta (`root_fingerprint`), but strips next_actions.
    /// - "full": includes meta/diagnostics and next_actions (when applicable).
    #[schemars(description = "Response mode: 'minimal' (default), 'facts', or 'full'")]
    pub response_mode: Option<ResponseMode>,

    /// Allow searching potential secret files (default: false).
    ///
    /// When true, the tool uses filesystem scanning even when a corpus exists, so patterns can
    /// match secret locations (e.g. `.env`). When false, secret paths are skipped.
    #[schemars(description = "Allow searching potential secret files (default: false).")]
    pub allow_secrets: Option<bool>,

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) root_hash: Option<u64>,
    pub(in crate::tools) pattern: String,
    /// Default page size for cursor-only continuation (0 means unspecified / legacy cursor).
    #[serde(default)]
    pub(in crate::tools) max_results: usize,
    /// Default budget for cursor-only continuation (0 means unspecified / legacy cursor).
    #[serde(default)]
    pub(in crate::tools) max_chars: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) file_pattern: Option<String>,
    pub(in crate::tools) case_sensitive: bool,
    pub(in crate::tools) whole_word: bool,
    #[serde(default)]
    pub(in crate::tools) allow_secrets: bool,
    #[serde(flatten)]
    pub(in crate::tools) mode: TextSearchCursorModeV1,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TextSearchResult {
    pub pattern: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scanned_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_large_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returned: Option<usize>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<BudgetTruncation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_actions: Option<Vec<ToolNextAction>>,
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
