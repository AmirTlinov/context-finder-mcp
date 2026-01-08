use rmcp::schemars;
use serde::Deserialize;

use super::response_mode::ResponseMode;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContextPackRequest {
    /// Search query
    #[schemars(description = "Natural language search query")]
    pub query: String,

    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

    /// Maximum primary results (default: 10)
    #[schemars(description = "Maximum number of primary results")]
    pub limit: Option<usize>,

    /// Maximum total characters for packed output (default: 2000)
    #[schemars(description = "Maximum total characters in packed output")]
    pub max_chars: Option<usize>,

    /// Optional include path prefixes (relative to project root). When provided, only matching
    /// paths are eligible for primary/related items.
    #[schemars(description = "Optional include path prefixes (relative to project root)")]
    pub include_paths: Option<Vec<String>>,

    /// Optional exclude path prefixes (relative to project root). Exclusions win over includes.
    #[schemars(description = "Optional exclude path prefixes (relative to project root)")]
    pub exclude_paths: Option<Vec<String>>,

    /// Optional file path filter (glob or substring). If no glob metachars are present, treated
    /// as substring match against the relative file path.
    #[schemars(description = "Optional file path filter (glob or substring)")]
    pub file_pattern: Option<String>,

    /// Related chunks per primary (default: 3)
    #[schemars(description = "Maximum related chunks per primary")]
    pub max_related_per_primary: Option<usize>,

    /// Prefer code results over markdown docs (implementation-first).
    #[schemars(description = "Prefer code results over markdown docs (implementation-first)")]
    pub prefer_code: Option<bool>,

    /// Whether markdown docs (e.g. *.md) may be included in the pack (default: true).
    #[schemars(description = "Whether markdown docs (e.g. *.md) may be included in the pack")]
    pub include_docs: Option<bool>,

    /// Related context mode: "explore" (default) or "focus" (query-gated).
    #[schemars(description = "Related context mode: 'explore' (default) or 'focus' (query-gated)")]
    pub related_mode: Option<String>,

    /// Search strategy: direct, extended, deep
    #[schemars(
        description = "Graph traversal depth: direct (none), extended (1-hop), deep (2-hop)"
    )]
    pub strategy: Option<String>,

    /// Graph language: rust, python, javascript, typescript
    #[schemars(description = "Programming language for graph analysis")]
    pub language: Option<String>,

    /// Response mode:
    /// - "facts" (default): keeps meta/index_state for freshness, strips next_actions to reduce noise.
    /// - "full": includes meta/index_state and next_actions.
    /// - "minimal": strips meta/index_state and next_actions to reduce noise. When not "full", `trace` is ignored.
    #[schemars(description = "Response mode: 'facts' (default), 'full', or 'minimal'")]
    pub response_mode: Option<ResponseMode>,

    /// Include debug output (adds a second MCP content block with debug JSON)
    #[schemars(description = "Include debug output as an additional response block")]
    pub trace: Option<bool>,

    /// Automatically build/refresh the semantic index when needed.
    ///
    /// When true, this tool may spend a bounded time budget to (re)index a missing/stale project.
    /// When false, the tool will not attempt auto-index and will fall back to lexical strategies.
    #[schemars(
        description = "Automatically build or refresh the semantic index before executing (default: true)."
    )]
    pub auto_index: Option<bool>,

    /// Auto-index time budget in milliseconds when auto_index=true.
    #[schemars(description = "Auto-index time budget in milliseconds (default: 15000).")]
    pub auto_index_budget_ms: Option<u64>,
}
