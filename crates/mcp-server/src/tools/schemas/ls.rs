use context_indexer::ToolMeta;
use context_protocol::BudgetTruncation;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;
use super::ToolNextAction;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LsRequest {
    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_ROOT/CONTEXT_PROJECT_ROOT; non-daemon fallback: cwd). Note: `path` sets the project root; to list a subdirectory, prefer `dir` (and in established sessions, ls treats a relative `path` as `dir` when `dir` is omitted)."
    )]
    pub path: Option<String>,

    /// Directory to list (relative to project root). Default: "."
    #[schemars(description = "Directory to list (relative to project root). Default: '.'")]
    pub dir: Option<String>,

    /// Include hidden entries (dotfiles). Default: true.
    #[schemars(description = "Include hidden entries (dotfiles). Default: true.")]
    pub all: Option<bool>,

    /// Allow listing potential secret entry names (default: true).
    ///
    /// This only affects *names* (not file contents). It exists to prevent agents from falling
    /// back to shell `ls` just to see whether secret files exist.
    #[schemars(
        description = "Allow listing potential secret entry names (default: true). Names-only."
    )]
    pub allow_secrets: Option<bool>,

    /// Maximum number of entries to return (default: 200)
    #[schemars(description = "Maximum number of directory entries to return (bounded).")]
    pub limit: Option<usize>,

    /// Hard `max_chars` budget for the `.context` response (including envelope).
    #[schemars(
        description = "Hard max_chars budget for the .context response (including envelope)."
    )]
    pub max_chars: Option<usize>,

    /// Response mode: "minimal" (default), "facts", or "full"
    #[schemars(description = "Response mode: 'minimal' (default), 'facts', or 'full'")]
    pub response_mode: Option<ResponseMode>,

    /// Opaque cursor token to continue a previous response
    #[schemars(description = "Opaque cursor token to continue a previous ls response")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::tools) struct LsCursorV1 {
    pub(in crate::tools) v: u32,
    pub(in crate::tools) tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) root_hash: Option<u64>,
    pub(in crate::tools) dir: String,
    #[serde(default)]
    pub(in crate::tools) all: bool,
    #[serde(default)]
    pub(in crate::tools) allow_secrets: bool,
    /// Default page size for cursor-only continuation (0 means unspecified / legacy cursor).
    #[serde(default)]
    pub(in crate::tools) limit: usize,
    /// Default budget for cursor-only continuation (0 means unspecified / legacy cursor).
    #[serde(default)]
    pub(in crate::tools) max_chars: usize,
    pub(in crate::tools) last_entry: String,
}

pub type LsTruncation = BudgetTruncation;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LsResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
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
    pub truncation: Option<LsTruncation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_actions: Option<Vec<ToolNextAction>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
    pub entries: Vec<String>,
}
