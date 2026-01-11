use context_indexer::ToolMeta;
use context_protocol::{BudgetTruncation, ToolNextAction};
use rmcp::schemars;
use serde::Serialize;

use super::response_mode::ResponseMode;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct MeaningFocusRequest {
    /// Project directory path.
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

    /// Repo-relative file or directory path to focus on.
    #[schemars(description = "Repo-relative file or directory path to focus on.")]
    pub focus: String,

    /// Optional natural-language query describing what to orient on (default: derived from focus).
    #[schemars(description = "Optional natural-language query describing what to orient on.")]
    pub query: Option<String>,

    /// Directory depth for grouping (default: 2).
    #[schemars(description = "Directory depth for grouping (1-4)")]
    pub map_depth: Option<usize>,

    /// Maximum number of directories to return (default: 12).
    #[schemars(description = "Maximum number of map entries returned")]
    pub map_limit: Option<usize>,

    /// Maximum UTF-8 characters for the entire meaning pack (default: 2000).
    #[schemars(description = "Maximum number of UTF-8 characters for the meaning pack")]
    pub max_chars: Option<usize>,

    /// Response mode:
    /// - "facts" (default): keeps meta/index_state for freshness, strips next_actions to reduce noise.
    /// - "full": includes meta/index_state and next_actions (when applicable).
    /// - "minimal": strips meta/index_state and next_actions to reduce noise.
    #[schemars(description = "Response mode: 'facts' (default), 'full', or 'minimal'")]
    pub response_mode: Option<ResponseMode>,

    /// Automatically build/refresh the semantic index when needed.
    #[schemars(
        description = "Automatically build or refresh the semantic index before executing (default: true)."
    )]
    pub auto_index: Option<bool>,

    /// Auto-index time budget in milliseconds when auto_index=true.
    #[schemars(description = "Auto-index time budget in milliseconds (default: 15000).")]
    pub auto_index_budget_ms: Option<u64>,
}

pub type MeaningFocusTruncation = BudgetTruncation;
pub type MeaningFocusNextAction = ToolNextAction;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MeaningFocusBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<MeaningFocusTruncation>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MeaningFocusResult {
    pub version: u32,
    pub query: String,
    pub format: String,
    pub pack: String,
    pub budget: MeaningFocusBudget,
    pub next_actions: Vec<MeaningFocusNextAction>,
    #[serde(default)]
    pub meta: ToolMeta,
}
