use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::file_slice::FileSliceResult;
use super::grep_context::GrepContextResult;
use super::repo_onboarding_pack::RepoOnboardingPackResult;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadPackIntent {
    Auto,
    File,
    Grep,
    Query,
    Onboarding,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadPackRequest {
    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

    /// What kind of pack to build (default: auto)
    #[schemars(description = "What kind of pack to build (auto/file/grep/query/onboarding)")]
    pub intent: Option<ReadPackIntent>,

    /// File path (relative to project root) when intent=file
    #[schemars(description = "File path (relative to project root)")]
    pub file: Option<String>,

    /// Regex pattern when intent=grep
    #[schemars(description = "Regex pattern to search for")]
    pub pattern: Option<String>,

    /// Natural language query when intent=query
    #[schemars(description = "Natural language query")]
    pub query: Option<String>,

    /// Optional file path filter for grep (glob or substring)
    #[schemars(description = "Optional file path filter (glob or substring)")]
    pub file_pattern: Option<String>,

    /// Regex context lines before a match (default: 20)
    #[schemars(description = "Number of context lines before each match")]
    pub before: Option<usize>,

    /// Regex context lines after a match (default: 20)
    #[schemars(description = "Number of context lines after each match")]
    pub after: Option<usize>,

    /// Case-sensitive regex matching (default: true)
    #[schemars(description = "Whether regex matching is case-sensitive")]
    pub case_sensitive: Option<bool>,

    /// First line to include (1-based, default: 1) when intent=file and cursor is not provided
    #[schemars(description = "First line to include (1-based) for file intent")]
    pub start_line: Option<usize>,

    /// Maximum number of lines to return for file intent (default: 200)
    #[schemars(description = "Maximum number of lines to return for file intent")]
    pub max_lines: Option<usize>,

    /// Maximum number of UTF-8 characters for the underlying result (default: 20000)
    #[schemars(description = "Maximum number of UTF-8 characters for the underlying result")]
    pub max_chars: Option<usize>,

    /// Timeout budget for building the pack in milliseconds (default: 55000)
    #[schemars(description = "Timeout budget for building the pack in milliseconds")]
    pub timeout_ms: Option<u64>,

    /// Opaque cursor token to continue a previous response
    #[schemars(description = "Opaque cursor token to continue a previous read_pack response")]
    pub cursor: Option<String>,

    /// Prefer code over docs when intent=query (default: true)
    #[schemars(description = "Prefer code over docs when building a context_pack")]
    pub prefer_code: Option<bool>,

    /// Include markdown docs when intent=query (default: true)
    #[schemars(description = "Whether to include docs in primary/related results")]
    pub include_docs: Option<bool>,

    /// Automatically build or refresh the semantic index before intent=query (default: true)
    #[schemars(
        description = "Automatically build or refresh the semantic index before intent=query (default: true)."
    )]
    pub auto_index: Option<bool>,

    /// Auto-index time budget in milliseconds for intent=query (default: 3000)
    #[schemars(
        description = "Auto-index time budget in milliseconds for intent=query (default: 3000)."
    )]
    pub auto_index_budget_ms: Option<u64>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReadPackNextAction {
    pub tool: String,
    pub args: serde_json::Value,
    pub reason: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadPackTruncation {
    MaxChars,
    Timeout,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReadPackBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<ReadPackTruncation>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReadPackSection {
    FileSlice {
        result: FileSliceResult,
    },
    GrepContext {
        result: GrepContextResult,
    },
    ContextPack {
        result: serde_json::Value,
    },
    RepoOnboardingPack {
        result: Box<RepoOnboardingPackResult>,
    },
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReadPackResult {
    pub version: u32,
    pub intent: ReadPackIntent,
    pub root: String,
    pub sections: Vec<ReadPackSection>,
    pub next_actions: Vec<ReadPackNextAction>,
    pub budget: ReadPackBudget,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
}
