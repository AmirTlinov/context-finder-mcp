use context_indexer::ToolMeta;
use context_protocol::{BudgetTruncation, ToolNextAction};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::file_slice::FileSliceResult;
use super::map::MapResult;
use super::response_mode::ResponseMode;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RepoOnboardingPackRequest {
    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_ROOT/CONTEXT_PROJECT_ROOT (legacy: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT); non-daemon fallback: cwd)."
    )]
    pub path: Option<String>,

    /// Directory depth for aggregation (default: 2)
    #[schemars(description = "Directory depth for grouping (1-4)")]
    pub map_depth: Option<usize>,

    /// Maximum number of directories to return (default: 20)
    #[schemars(description = "Limit number of map nodes returned")]
    pub map_limit: Option<usize>,

    /// Optional explicit doc file paths to include (relative to project root). If omitted, uses a
    /// built-in prioritized list (AGENTS/README/QUICK_START/contracts/...).
    #[schemars(
        description = "Optional explicit doc file paths to include (relative to project root)"
    )]
    pub doc_paths: Option<Vec<String>>,

    /// Maximum number of docs to include (default: 8)
    #[schemars(description = "Maximum number of docs to include (bounded)")]
    pub docs_limit: Option<usize>,

    /// Max lines per doc slice (default: 200)
    #[schemars(description = "Max lines per doc slice")]
    pub doc_max_lines: Option<usize>,

    /// Max chars per doc slice (default: 6000)
    #[schemars(description = "Max UTF-8 chars per doc slice")]
    pub doc_max_chars: Option<usize>,

    /// Maximum number of UTF-8 characters for the entire onboarding pack (default: 6000)
    #[schemars(description = "Maximum number of UTF-8 characters for the onboarding pack")]
    pub max_chars: Option<usize>,

    /// Response mode:
    /// - "facts" (default): keeps meta/index_state for freshness, strips next_actions to reduce noise.
    /// - "full": includes meta/index_state and next_actions (when applicable).
    /// - "minimal": strips meta/index_state and next_actions to reduce noise.
    #[schemars(description = "Response mode: 'facts' (default), 'full', or 'minimal'")]
    pub response_mode: Option<ResponseMode>,

    /// Automatically build/refresh the semantic index when needed.
    ///
    /// Repo onboarding packs are often the first tool call in a fresh session; allowing a bounded
    /// auto-index here helps subsequent semantic tools (search/context_pack) become available
    /// without a separate explicit indexing step.
    #[schemars(
        description = "Automatically build or refresh the semantic index before executing (default: true)."
    )]
    pub auto_index: Option<bool>,

    /// Auto-index time budget in milliseconds when auto_index=true.
    #[schemars(description = "Auto-index time budget in milliseconds (default: 15000).")]
    pub auto_index_budget_ms: Option<u64>,
}

pub type RepoOnboardingPackTruncation = BudgetTruncation;

#[derive(Debug, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepoOnboardingDocsReason {
    DocsLimitZero,
    NoDocCandidates,
    DocsNotFound,
    MaxChars,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RepoOnboardingPackBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<RepoOnboardingPackTruncation>,
}

pub type RepoOnboardingNextAction = ToolNextAction;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RepoOnboardingPackResult {
    pub version: u32,
    pub root: String,
    pub map: MapResult,
    pub docs: Vec<FileSliceResult>,
    #[serde(default)]
    pub omitted_doc_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs_reason: Option<RepoOnboardingDocsReason>,
    pub next_actions: Vec<RepoOnboardingNextAction>,
    pub budget: RepoOnboardingPackBudget,
    #[serde(default)]
    pub meta: ToolMeta,
}
