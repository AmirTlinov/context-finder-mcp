use context_protocol::BudgetTruncation;
use serde::{Deserialize, Serialize};

/// Evidence pointer (EV): minimal, verifiable reference to exact source material.
///
/// This is an internal canonical model; tool/CLI adapters can format it as CP lines.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidencePointer {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MeaningPackRequest {
    /// Natural-language query describing what to orient on.
    pub query: String,
    /// Directory depth for grouping (default: 2; clamped to 1..=4).
    pub map_depth: Option<usize>,
    /// Maximum number of map entries returned (default: 12).
    pub map_limit: Option<usize>,
    /// Maximum UTF-8 characters for the entire meaning pack (default: 2000).
    pub max_chars: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeaningPackBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<BudgetTruncation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeaningPackResult {
    pub version: u32,
    pub query: String,
    pub format: String,
    pub pack: String,
    pub budget: MeaningPackBudget,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MeaningFocusRequest {
    /// Repo-relative file or directory path to focus on.
    pub focus: String,
    /// Optional natural-language query describing what to orient on (default: derived from focus).
    pub query: Option<String>,
    /// Directory depth for grouping (default: 2; clamped to 1..=4).
    pub map_depth: Option<usize>,
    /// Maximum number of map entries returned (default: 12).
    pub map_limit: Option<usize>,
    /// Maximum UTF-8 characters for the entire meaning pack (default: 2000).
    pub max_chars: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeaningFocusBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<BudgetTruncation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeaningFocusResult {
    pub version: u32,
    pub query: String,
    pub format: String,
    pub pack: String,
    pub budget: MeaningFocusBudget,
}
