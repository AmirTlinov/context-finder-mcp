use context_indexer::ToolMeta;
use context_protocol::BudgetTruncation;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;
use super::ToolNextAction;
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

    /// Hard `max_chars` budget for the `.context` response (including envelope).
    ///
    /// The budget is spent mostly on file paths, but the tool reserves headroom for the envelope
    /// and (when applicable) a continuation cursor. Under extremely small budgets the tool may
    /// return few or no paths, but it avoids failing solely due to `max_chars`.
    #[schemars(
        description = "Hard max_chars budget for the .context response (including envelope)."
    )]
    pub max_chars: Option<usize>,

    /// Response mode:
    /// - "minimal" (default): lowest noise; strips most diagnostics and next_actions, but keeps provenance meta (`root_fingerprint`).
    /// - "facts": payload-focused; keeps lightweight counters/budget info and provenance meta (`root_fingerprint`), but strips next_actions.
    /// - "full": includes meta/diagnostics (freshness index_state) and next_actions (when applicable).
    #[schemars(description = "Response mode: 'minimal' (default), 'facts', or 'full'")]
    pub response_mode: Option<ResponseMode>,

    /// Allow listing potential secret file paths (default: false).
    ///
    /// When false, the tool omits common secret locations (e.g. `.env`, SSH keys, `*.pem`/`*.key`)
    /// to reduce accidental leakage in agent context windows.
    #[schemars(description = "Allow listing potential secret file paths (default: false).")]
    pub allow_secrets: Option<bool>,

    /// Opaque cursor token to continue a previous response
    #[schemars(description = "Opaque cursor token to continue a previous list_files response")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::tools) struct ListFilesCursorV1 {
    pub(in crate::tools) v: u32,
    pub(in crate::tools) tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) root_hash: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) file_pattern: Option<String>,
    /// Default page size for cursor-only continuation (0 means unspecified / legacy cursor).
    #[serde(default)]
    pub(in crate::tools) limit: usize,
    /// Default budget for cursor-only continuation (0 means unspecified / legacy cursor).
    #[serde(default)]
    pub(in crate::tools) max_chars: usize,
    #[serde(default)]
    pub(in crate::tools) allow_secrets: bool,
    pub(in crate::tools) last_file: String,
}

pub type ListFilesTruncation = BudgetTruncation;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListFilesResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scanned_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returned: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<usize>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<ListFilesTruncation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_actions: Option<Vec<ToolNextAction>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
    pub files: Vec<String>,
}
