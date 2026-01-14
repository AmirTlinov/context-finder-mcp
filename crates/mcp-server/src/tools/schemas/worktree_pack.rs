use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;
use super::ToolNextAction;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorktreePackRequest {
    /// Project directory path (defaults to session root; fallback: env/git/cwd).
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

    /// Optional query to help rank worktrees (e.g., \"which worktree contains X?\").
    #[schemars(description = "Optional query used for ranking/labeling worktrees.")]
    pub query: Option<String>,

    /// Maximum number of worktrees to return per page (default: 20).
    #[schemars(description = "Maximum number of worktrees to return per page (bounded).")]
    pub limit: Option<usize>,

    /// Hard `max_chars` budget for the `.context` response (including envelope).
    #[schemars(
        description = "Hard max_chars budget for the .context response (including envelope)."
    )]
    pub max_chars: Option<usize>,

    /// Response mode:
    /// - \"minimal\" (default): lowest noise; strips most diagnostics and next_actions, but keeps provenance meta (`root_fingerprint`).
    /// - \"facts\": payload-focused; keeps lightweight counters/structure and provenance meta (`root_fingerprint`), but strips next_actions.
    /// - \"full\": includes meta/diagnostics (freshness index_state) and next_actions (when applicable).
    #[schemars(description = "Response mode: 'minimal' (default), 'facts', or 'full'")]
    pub response_mode: Option<ResponseMode>,

    /// Opaque cursor token to continue a previous response.
    #[schemars(description = "Opaque cursor token to continue a previous worktree_pack response.")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::tools) struct WorktreePackCursorV1 {
    pub(in crate::tools) v: u32,
    pub(in crate::tools) tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) root_hash: Option<u64>,
    #[serde(default)]
    pub(in crate::tools) limit: usize,
    pub(in crate::tools) offset: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) query: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct WorktreeInfo {
    /// Worktree path. Prefer absolute paths so follow-up tool calls can pass it verbatim.
    pub path: String,
    /// Best-effort display path (relative to the requested root when possible).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_path: Option<String>,
    /// Worktree name (best-effort basename).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Branch name (short, when available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// HEAD short hash (when available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// HEAD subject line (when available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_subject: Option<String>,
    /// Whether the worktree has uncommitted changes (best-effort).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    /// Sample of modified paths (best-effort, bounded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty_paths: Option<Vec<String>>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct WorktreePackResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_worktrees: Option<usize>,
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
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_actions: Option<Vec<ToolNextAction>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
    pub worktrees: Vec<WorktreeInfo>,
}
