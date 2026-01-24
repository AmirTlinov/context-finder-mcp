use context_indexer::ToolMeta;
use context_protocol::{BudgetTruncation, ToolNextAction};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;
use super::worktree_pack::WorktreeInfo;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AtlasPackRequest {
    /// Project directory path (defaults to session root; fallback: env (non-daemon: cwd)).
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_ROOT/CONTEXT_PROJECT_ROOT (legacy: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT); non-daemon fallback: cwd)."
    )]
    pub path: Option<String>,

    /// Optional natural-language query (what to orient on). When omitted, uses an onboarding default.
    #[schemars(description = "Optional natural-language query used to focus the atlas.")]
    pub query: Option<String>,

    /// Maximum number of worktrees to include (default: 10; bounded).
    #[schemars(description = "Maximum number of worktrees to include in the atlas (bounded).")]
    pub worktree_limit: Option<usize>,

    /// Hard `max_chars` budget for the `.context` response (including envelope, default: 6000).
    #[schemars(
        description = "Hard max_chars budget for the .context response (including envelope)."
    )]
    pub max_chars: Option<usize>,

    /// Response mode:
    /// - "facts" (default): CP pack + lightweight summary, strips next_actions.
    /// - "full": includes next_actions (drill-down + evidence_fetch) and richer worktree summaries.
    /// - "minimal": strips most meta/diagnostics and next_actions (lowest noise).
    #[schemars(description = "Response mode: 'facts' (default), 'full', or 'minimal'")]
    pub response_mode: Option<ResponseMode>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AtlasPackBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<BudgetTruncation>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AtlasPackResult {
    pub version: u32,
    /// Resolved query used for meaning extraction.
    pub query: String,
    /// Format marker for the embedded meaning pack (CPV1).
    pub meaning_format: String,
    /// Meaning budget (engine-side, CPV1 only).
    pub meaning_max_chars: usize,
    pub meaning_used_chars: usize,
    pub meaning_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meaning_truncation: Option<BudgetTruncation>,
    /// Meaning pack (CPV1): evidence-backed “map of sense”.
    pub meaning_pack: String,
    /// Worktree list (bounded); in full mode may include per-worktree purpose summaries.
    pub worktrees: Vec<WorktreeInfo>,
    pub worktrees_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktrees_next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_actions: Option<Vec<ToolNextAction>>,
    pub budget: AtlasPackBudget,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
}
