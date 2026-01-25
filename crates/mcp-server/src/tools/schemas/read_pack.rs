use context_indexer::ToolMeta;
use context_protocol::{BudgetTruncation, ToolNextAction};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::file_slice::FileSliceResult;
use super::grep_context::GrepContextResult;
use super::overview::OverviewResult;
use super::repo_onboarding_pack::RepoOnboardingPackResult;
use super::response_mode::ResponseMode;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadPackIntent {
    Auto,
    File,
    Grep,
    Query,
    Onboarding,
    Memory,
    Recall,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadPackRequest {
    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_ROOT/CONTEXT_PROJECT_ROOT; non-daemon fallback: cwd). DX: when a session root is already set and `file`/`file_pattern` are omitted, a relative `path` is treated as a file/file_pattern hint instead of switching the project root. Use `root_set` for explicit project switching."
    )]
    pub path: Option<String>,

    /// What kind of pack to build (default: auto)
    #[schemars(
        description = "What kind of pack to build (auto/file/grep/query/onboarding/memory/recall)"
    )]
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

    /// One-call recall: a free-form question/prompt (used when intent=recall, or when intent=auto and ask/questions are provided)
    #[schemars(
        description = "One-call recall prompt (intent=recall, or auto when ask/questions are present)."
    )]
    pub ask: Option<String>,

    /// One-call recall: multiple focused questions (preferred over ask for deterministic output)
    #[schemars(
        description = "One-call recall questions (preferred over ask for deterministic output)."
    )]
    pub questions: Option<Vec<String>>,

    /// Optional recall topics/hints (best-effort)
    #[schemars(description = "Optional recall topics/hints (best-effort).")]
    pub topics: Option<Vec<String>>,

    /// Optional file path filter for grep (glob or substring)
    #[schemars(
        description = "Optional file path filter (glob or substring); applies to grep and query intents"
    )]
    pub file_pattern: Option<String>,

    /// Optional include path prefixes (relative to project root). When provided, only matching
    /// paths are eligible for query intent packs.
    #[schemars(description = "Optional include path prefixes (relative to project root)")]
    pub include_paths: Option<Vec<String>>,

    /// Optional exclude path prefixes (relative to project root). Exclusions win over includes.
    #[schemars(description = "Optional exclude path prefixes (relative to project root)")]
    pub exclude_paths: Option<Vec<String>>,

    /// Regex context lines before a match (default: 20)
    #[schemars(description = "Number of context lines before each match")]
    pub before: Option<usize>,

    /// Regex context lines after a match (default: 20)
    #[schemars(description = "Number of context lines after each match")]
    pub after: Option<usize>,

    /// Case-sensitive regex matching (default: true)
    #[schemars(description = "Whether regex matching is case-sensitive")]
    pub case_sensitive: Option<bool>,

    /// First line to include (1-based, default: 1) when intent=file and cursor is not provided. Alias: offset
    #[schemars(description = "First line to include (1-based) for file intent. Alias: offset")]
    #[serde(alias = "offset")]
    pub start_line: Option<usize>,

    /// Maximum number of lines to return for file intent (default: 200). Alias: limit
    #[schemars(description = "Maximum number of lines to return for file intent. Alias: limit")]
    #[serde(alias = "limit")]
    pub max_lines: Option<usize>,

    /// Maximum number of UTF-8 characters for the underlying result (default: 6000)
    #[schemars(description = "Maximum number of UTF-8 characters for the underlying result")]
    pub max_chars: Option<usize>,

    /// Response mode:
    /// - "facts" (default): payload-focused output; keeps provenance meta (`root_fingerprint`) but strips next_actions to reduce noise.
    /// - "full": includes meta/diagnostics (freshness index_state) and next_actions (when applicable).
    /// - "minimal": smallest possible output; strips helper fields and next_actions, but keeps provenance meta (`root_fingerprint`).
    #[schemars(description = "Response mode: 'facts' (default), 'full', or 'minimal'")]
    pub response_mode: Option<ResponseMode>,

    /// Allow reading/searching potential secret files (default: false).
    ///
    /// When false, read_pack refuses direct secret file reads (intent=file) and skips secret paths
    /// in grep/text fallbacks to prevent accidental leakage in agent context windows.
    #[schemars(description = "Allow reading/searching potential secret files (default: false).")]
    pub allow_secrets: Option<bool>,

    /// Timeout budget for building the pack in milliseconds (default: 12000)
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
}

pub type ReadPackNextAction = ToolNextAction;
pub type ReadPackTruncation = BudgetTruncation;

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
    ProjectFacts {
        result: ProjectFactsResult,
    },
    ExternalMemory {
        result: ReadPackExternalMemoryResult,
    },
    Snippet {
        result: ReadPackSnippet,
    },
    Recall {
        result: ReadPackRecallResult,
    },
    Overview {
        result: OverviewResult,
    },
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

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Clone)]
pub struct ReadPackExternalMemoryResult {
    /// Source system for the external memory overlay (e.g. "branchmind").
    pub source: String,
    /// Optional relative path under the project root where the memory was loaded from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Ranked memory hits, bounded and low-noise.
    pub hits: Vec<ReadPackExternalMemoryHit>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Clone)]
pub struct ReadPackExternalMemoryHit {
    /// High-level category (e.g. "decision", "blocker", "evidence", "note", "trace").
    pub kind: String,
    /// Optional short title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Relevance score (higher is better).
    pub score: f32,
    /// Optional last-updated timestamp (unix ms) from the source system.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_ms: Option<u64>,
    /// Bounded excerpt for agent display.
    pub excerpt: String,
    /// Optional structured reference payload for deep-linking back to the source system.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadPackSnippetKind {
    Code,
    Doc,
    Config,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Clone)]
pub struct ReadPackSnippet {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ReadPackSnippetKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Cursor to continue reading the same file window (cursor-only continuation via read_pack).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Clone)]
pub struct ReadPackRecallResult {
    pub question: String,
    pub snippets: Vec<ReadPackSnippet>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Clone)]
pub struct ProjectFactsResult {
    pub version: u32,
    /// Primary ecosystems detected from common marker files (best-effort, bounded).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ecosystems: Vec<String>,
    /// Build/task tooling detected from common marker files (best-effort, bounded).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub build_tools: Vec<String>,
    /// CI/CD tooling detected from common marker files (best-effort, bounded).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ci: Vec<String>,
    /// Contract surfaces detected (OpenAPI/proto/schemas). Values are relative paths or short labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contracts: Vec<String>,
    /// Key top-level directories worth knowing (best-effort, bounded).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_dirs: Vec<String>,
    /// Workspace/module roots (monorepo packages, crates, apps) as relative paths (best-effort, bounded).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<String>,
    /// Entry point candidates (relative file paths, bounded).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entry_points: Vec<String>,
    /// Key config files worth reading first (relative file paths, bounded).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_configs: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReadPackResult {
    pub version: u32,
    pub intent: ReadPackIntent,
    pub root: String,
    pub sections: Vec<ReadPackSection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_actions: Vec<ReadPackNextAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub budget: ReadPackBudget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
}
