use context_indexer::ToolMeta;
use context_protocol::{BudgetTruncation, ToolNextAction};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
pub struct EvidencePointer {
    /// Repo-relative file path.
    pub file: String,
    /// 1-based start line (inclusive).
    pub start_line: usize,
    /// 1-based end line (inclusive).
    pub end_line: usize,
    /// Optional file hash for stale detection.
    pub source_hash: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EvidenceFetchRequest {
    /// Project directory path.
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

    /// Evidence pointers to fetch (bounded).
    pub items: Vec<EvidencePointer>,

    /// Maximum UTF-8 characters for the entire response (default: 2000).
    pub max_chars: Option<usize>,

    /// Maximum lines per evidence item (default: 200).
    pub max_lines: Option<usize>,

    /// When true, treat source_hash mismatches as an error (default: false).
    pub strict_hash: Option<bool>,

    /// Response mode:
    /// - "facts" (default): keeps meta/index_state for freshness, strips next_actions to reduce noise.
    /// - "full": includes meta/index_state and next_actions (when applicable).
    /// - "minimal": strips meta/index_state and next_actions to reduce noise.
    pub response_mode: Option<ResponseMode>,
}

pub type EvidenceFetchTruncation = BudgetTruncation;
pub type EvidenceFetchNextAction = ToolNextAction;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EvidenceFetchBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<EvidenceFetchTruncation>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EvidenceFetchItem {
    pub evidence: EvidencePointer,
    pub content: String,
    pub truncated: bool,
    #[serde(default)]
    pub stale: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EvidenceFetchResult {
    pub version: u32,
    pub items: Vec<EvidenceFetchItem>,
    pub budget: EvidenceFetchBudget,
    pub next_actions: Vec<EvidenceFetchNextAction>,
    #[serde(default)]
    pub meta: ToolMeta,
}
