use context_indexer::ToolMeta;
use context_protocol::BudgetTruncation;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::content_format::ContentFormat;
use super::response_mode::ResponseMode;
use super::ToolNextAction;
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GrepContextRequest {
    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

    /// Pattern to search for. Optional when continuing via `cursor`.
    /// By default treated as a Rust regex (unless `literal: true`).
    #[schemars(
        description = "Pattern to search for. Optional when continuing via cursor. Note: by default this is a Rust regex; when sending JSON, escape backslashes (e.g. `\\\\(` to match a literal `(`)."
    )]
    pub pattern: Option<String>,

    /// Treat `pattern` as a literal string (like `rg -F`) instead of a regex.
    #[schemars(description = "If true, treat pattern as a literal string (no regex parsing)")]
    pub literal: Option<bool>,

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

    /// Hard `max_chars` budget for the `.context` response (including envelope).
    ///
    /// The tool will truncate hunks as needed to stay within budget and (when applicable) return a
    /// cursor so the agent can continue pagination. Under extremely small budgets the tool may
    /// return few or no hunks, but it avoids failing solely due to `max_chars`.
    #[schemars(
        description = "Hard max_chars budget for the .context response (including envelope)."
    )]
    pub max_chars: Option<usize>,

    /// Case-sensitive regex matching (default: true)
    #[schemars(description = "Whether regex matching is case-sensitive")]
    pub case_sensitive: Option<bool>,

    /// Render format for hunk content.
    ///
    /// Default is optimized for agent context windows:
    /// - `plain` for low-noise modes (`response_mode=facts|minimal`)
    /// - `numbered` for debug-rich output (`response_mode=full`)
    #[schemars(
        description = "Render format for hunk content: 'plain' (low-noise default) or 'numbered' (full/debug)"
    )]
    pub format: Option<ContentFormat>,

    /// Response mode:
    /// - "minimal" (default): lowest noise; strips most diagnostics and next_actions, but keeps provenance meta (`root_fingerprint`).
    /// - "facts": payload-focused; keeps lightweight counters/budget info and provenance meta (`root_fingerprint`), but strips next_actions.
    /// - "full": includes meta/diagnostics (freshness index_state) and next_actions (when applicable).
    #[schemars(description = "Response mode: 'minimal' (default), 'facts', or 'full'")]
    pub response_mode: Option<ResponseMode>,

    /// Allow searching potential secret files (default: false).
    ///
    /// When false, the tool refuses explicit secret file reads and skips common secret paths
    /// (e.g. `.env`, SSH keys, `*.pem`/`*.key`) to prevent accidental leakage.
    #[schemars(description = "Allow searching potential secret files (default: false).")]
    pub allow_secrets: Option<bool>,

    /// Opaque cursor token to continue a previous response
    #[schemars(description = "Opaque cursor token to continue a previous rg response")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::tools) struct GrepContextCursorV1 {
    pub(in crate::tools) v: u32,
    pub(in crate::tools) tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) root_hash: Option<u64>,
    pub(in crate::tools) pattern: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) file_pattern: Option<String>,
    #[serde(default)]
    pub(in crate::tools) literal: bool,
    pub(in crate::tools) case_sensitive: bool,
    pub(in crate::tools) before: usize,
    pub(in crate::tools) after: usize,
    #[serde(default)]
    pub(in crate::tools) max_matches: usize,
    #[serde(default)]
    pub(in crate::tools) max_hunks: usize,
    #[serde(default)]
    pub(in crate::tools) max_chars: usize,
    #[serde(default = "default_grep_context_cursor_format")]
    pub(in crate::tools) format: ContentFormat,
    #[serde(default)]
    pub(in crate::tools) allow_secrets: bool,
    pub(in crate::tools) resume_file: String,
    pub(in crate::tools) resume_line: usize,
}

const fn default_grep_context_cursor_format() -> ContentFormat {
    ContentFormat::Numbered
}

pub type GrepContextTruncation = BudgetTruncation;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GrepContextHunk {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_lines: Option<Vec<usize>>,
    pub content: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GrepContextResult {
    pub pattern: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub case_sensitive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scanned_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returned_matches: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returned_hunks: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<usize>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<GrepContextTruncation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_actions: Option<Vec<ToolNextAction>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
    pub hunks: Vec<GrepContextHunk>,
}
