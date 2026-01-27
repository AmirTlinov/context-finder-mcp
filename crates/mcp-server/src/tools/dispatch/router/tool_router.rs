use super::super::*;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};

fn strip_structured_content(mut result: CallToolResult) -> CallToolResult {
    result.structured_content = None;
    result
}

pub(super) fn build_tool_router_with_param_hints(
) -> super::super::tool_router_hints::ToolRouterWithParamHints<ContextFinderService> {
    super::super::tool_router_hints::ToolRouterWithParamHints::new(
        ContextFinderService::tool_router(),
    )
}

#[tool_router]
impl ContextFinderService {
    /// Tool capabilities handshake (versions, budgets, start route).
    #[tool(
        description = "Return tool capabilities: versions, default budgets, and the recommended start route for zero-guesswork onboarding."
    )]
    pub async fn capabilities(
        &self,
        Parameters(request): Parameters<CapabilitiesRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::capabilities::capabilities(self, request).await?,
        ))
    }

    /// `.context` legend and tool usage notes.
    #[tool(
        description = "Explain the `.context` output legend (A/R/N/M) and recommended usage patterns. The only tool that returns a [LEGEND] block."
    )]
    pub async fn help(
        &self,
        Parameters(request): Parameters<HelpRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::help::help(self, request).await?,
        ))
    }

    /// Session root status (per-connection) + workspace roots (when available).
    #[tool(
        description = "Get the current per-connection session root and MCP workspace roots (if available). Useful for multi-root workspaces and avoiding cross-project mixups."
    )]
    pub async fn root_get(
        &self,
        Parameters(request): Parameters<RootGetRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::root::root_get(self, request).await?,
        ))
    }

    /// Explicitly set the per-connection session root.
    #[tool(
        description = "Explicitly set the per-connection session root. Use this to switch projects intentionally within one MCP session (or to disambiguate multi-root workspaces)."
    )]
    pub async fn root_set(
        &self,
        Parameters(request): Parameters<RootSetRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::root::root_set(self, request).await?,
        ))
    }

    /// Project structure overview (tree-like).
    #[tool(description = "Project structure overview with directories, files, and top symbols.")]
    pub async fn tree(
        &self,
        Parameters(request): Parameters<MapRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::map::map(self, request).await?,
        ))
    }

    /// Repo onboarding pack (map + key docs slices + next actions).
    #[tool(
        description = "Build a repo onboarding pack: map + key docs (via file slices) + next actions. Returns a single bounded `.context` response for fast project adoption."
    )]
    pub async fn repo_onboarding_pack(
        &self,
        Parameters(request): Parameters<RepoOnboardingPackRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::repo_onboarding_pack::repo_onboarding_pack(self, request).await?,
        ))
    }

    /// Meaning-first pack (facts-only map + evidence pointers, token-efficient).
    #[tool(
        description = "Meaning-first pack: returns a token-efficient Cognitive Pack (CP) with high-signal repo meaning (structure + candidates) and evidence pointers for on-demand verbatim reads."
    )]
    pub async fn meaning_pack(
        &self,
        Parameters(request): Parameters<MeaningPackRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::meaning_pack::meaning_pack(self, request).await?,
        ))
    }

    /// Meaning-first focus (semantic zoom): scoped candidates + evidence pointers.
    #[tool(
        description = "Meaning-first focus (semantic zoom): returns a token-efficient Cognitive Pack (CP) scoped to a file/dir, with evidence pointers for on-demand verbatim reads."
    )]
    pub async fn meaning_focus(
        &self,
        Parameters(request): Parameters<MeaningFocusRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::meaning_focus::meaning_focus(self, request).await?,
        ))
    }

    /// Worktree atlas: list git worktrees/branches and what is being worked on.
    #[tool(
        description = "Worktree atlas: list git worktrees/branches and what is being worked on (bounded, deterministic). Provides next actions to drill down via meaning tools."
    )]
    pub async fn worktree_pack(
        &self,
        Parameters(request): Parameters<WorktreePackRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::worktree_pack::worktree_pack(self, request).await?,
        ))
    }

    /// One-call atlas: meaning-first CP + worktree overview (onboarding-first, evidence-backed).
    #[tool(
        description = "One-call atlas for agent onboarding: meaning-first CP (canon loop, CI/contracts/entrypoints) + worktree overview. Evidence-backed, bounded, deterministic."
    )]
    pub async fn atlas_pack(
        &self,
        Parameters(request): Parameters<AtlasPackRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::atlas_pack::atlas_pack(self, request).await?,
        ))
    }

    /// Notebook pack: list saved anchors/runbooks (cross-session, low-noise).
    #[tool(
        description = "Agent notebook pack: list durable anchors and runbooks for a repo (cross-session continuity)."
    )]
    pub async fn notebook_pack(
        &self,
        Parameters(request): Parameters<NotebookPackRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::notebook_pack::notebook_pack(self, request).await?,
        ))
    }

    /// Notebook edit: upsert/delete anchors and runbooks (explicit writes).
    #[tool(
        description = "Agent notebook edit: upsert/delete anchors and runbooks (explicit, durable writes; fail-closed)."
    )]
    pub async fn notebook_edit(
        &self,
        Parameters(request): Parameters<NotebookEditRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::notebook_edit::notebook_edit(self, request).await?,
        ))
    }

    /// Notebook apply: one-click preview/apply/rollback for notebook_suggest output.
    #[tool(
        description = "Notebook apply: one-click preview/apply/rollback for notebook_suggest output (safe backup + rollback)."
    )]
    pub async fn notebook_apply_suggest(
        &self,
        Parameters(request): Parameters<NotebookApplySuggestRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::notebook_apply_suggest::notebook_apply_suggest(self, request).await?,
        ))
    }

    /// Notebook suggest: propose anchors + runbooks (read-only; evidence-backed).
    #[tool(
        description = "Notebook suggest: propose evidence-backed anchors and runbooks (read-only). Designed to reduce tool-call count; apply via notebook_apply_suggest."
    )]
    pub async fn notebook_suggest(
        &self,
        Parameters(request): Parameters<NotebookSuggestRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::notebook_suggest::notebook_suggest(self, request).await?,
        ))
    }

    /// Runbook pack: TOC by default, expand a section on demand (cursor-based).
    #[tool(
        description = "Runbook pack: returns a low-noise TOC by default, with freshness/staleness; expand sections on demand with cursor continuation."
    )]
    pub async fn runbook_pack(
        &self,
        Parameters(request): Parameters<RunbookPackRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::runbook_pack::runbook_pack(self, request).await?,
        ))
    }

    /// Bounded exact text search (literal substring), like `rg -F`.
    #[tool(
        description = "Search for an exact text pattern in project files with bounded output (like `rg -F`, but safe for agent context). Uses corpus if available, otherwise scans files without side effects."
    )]
    pub async fn text_search(
        &self,
        Parameters(request): Parameters<TextSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::text_search::text_search(self, request).await?,
        ))
    }

    /// Read a bounded slice of a file within the project root (cat-like, safe for agents).
    #[tool(
        description = "Read a bounded slice of a file (by line) within the project root. Safe replacement for `cat`/`sed -n`; enforces max_lines/max_chars and prevents path traversal."
    )]
    pub async fn cat(
        &self,
        Parameters(request): Parameters<FileSliceRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::file_slice::file_slice(self, &request).await?,
        ))
    }

    /// Fetch exact evidence spans (verbatim) referenced by meaning packs.
    #[tool(
        description = "Evidence fetch (verbatim): read exact line windows for one or more evidence pointers. Intended as the on-demand 'territory' step after meaning-first navigation."
    )]
    pub async fn evidence_fetch(
        &self,
        Parameters(request): Parameters<EvidenceFetchRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::evidence_fetch::evidence_fetch(self, request).await?,
        ))
    }

    /// Build a one-call semantic reading pack (cat / rg / context pack / onboarding / memory).
    #[tool(
        description = "One-call semantic reading pack. A cognitive facade over cat/rg/context_pack/repo_onboarding_pack (+ intent=memory for long-memory overview + key config/doc slices): returns the most relevant bounded slice(s) plus continuation cursors and next actions."
    )]
    pub async fn read_pack(
        &self,
        Parameters(request): Parameters<ReadPackRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::read_pack::read_pack(self, request).await?,
        ))
    }

    /// List directory entries (names-only, like `ls -a`).
    #[tool(
        description = "List directory entries (names-only, like `ls -a`) within the project root. Bounded output with cursor pagination; safe replacement for shell `ls` in agent loops."
    )]
    pub async fn ls(
        &self,
        Parameters(request): Parameters<LsRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::ls::ls(self, request).await?,
        ))
    }

    /// List project file paths (find-like).
    #[tool(
        description = "List project file paths (relative to project root), like `find`/`rg --files` but bounded + cursor-based. Use this when you need recursive paths; use `ls` for directory entries."
    )]
    pub async fn find(
        &self,
        Parameters(request): Parameters<ListFilesRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::list_files::list_files(self, request).await?,
        ))
    }

    /// Regex search with merged context hunks (rg-like).
    #[tool(
        description = "Search project files with a regex and return merged context hunks (N lines before/after). Designed to replace `rg -C/-A/-B` plus multiple cat calls with a single bounded response."
    )]
    pub async fn rg(
        &self,
        Parameters(request): Parameters<GrepContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::grep_context::grep_context(self, request).await?,
        ))
    }

    /// Regex search with merged context hunks (grep-like).
    #[tool(
        description = "Alias for `rg`. Search project files with a regex and return merged context hunks (N lines before/after)."
    )]
    pub async fn grep(
        &self,
        Parameters(request): Parameters<GrepContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::grep_context::grep_context(self, request).await?,
        ))
    }

    /// Execute multiple Context tools in a single call (agent-friendly batch).
    #[tool(
        description = "Execute multiple Context tools in one call. Returns a single bounded `.context` response with per-item status (partial success) and a global max_chars budget."
    )]
    pub async fn batch(
        &self,
        Parameters(request): Parameters<BatchRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::batch::batch(self, request).await?,
        ))
    }

    /// Diagnose model/GPU/index configuration
    #[tool(
        description = "Show diagnostics for model directory, CUDA/ORT runtime, and per-project index/corpus status. Use this when something fails (e.g., GPU provider missing)."
    )]
    pub async fn doctor(
        &self,
        Parameters(request): Parameters<DoctorRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::doctor::doctor(self, request).await?,
        ))
    }

    /// Semantic code search
    #[tool(
        description = "Search for code using natural language. Returns relevant code snippets with file locations and symbols."
    )]
    pub async fn search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::search::search(self, request).await?,
        ))
    }

    /// Search with graph context
    #[tool(
        description = "Search for code with automatic graph-based context. Returns code plus related functions/types through call graphs and dependencies. Best for understanding how code connects."
    )]
    pub async fn context(
        &self,
        Parameters(request): Parameters<ContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::context::context(self, request).await?,
        ))
    }

    /// Build a bounded context pack for agents (single-call context).
    #[tool(
        description = "Build a bounded context pack for a query: primary hits + graph-related halo, under a strict character budget. Intended as a single-call payload for AI agents."
    )]
    pub async fn context_pack(
        &self,
        Parameters(request): Parameters<ContextPackRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::context_pack::context_pack(self, request).await?,
        ))
    }

    /// Find all usages of a symbol (impact analysis)
    #[tool(
        description = "Find all places where a symbol is used. Essential for refactoring - shows direct usages, transitive dependencies, and related tests."
    )]
    pub async fn impact(
        &self,
        Parameters(request): Parameters<ImpactRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::impact::impact(self, request).await?,
        ))
    }

    /// Trace call path between two symbols
    #[tool(
        description = "Show call chain from one symbol to another. Essential for understanding code flow and debugging."
    )]
    pub async fn trace(
        &self,
        Parameters(request): Parameters<TraceRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::trace::trace(self, request).await?,
        ))
    }

    /// Deep dive into a symbol
    #[tool(
        description = "Get complete information about a symbol: definition, dependencies, dependents, tests, and documentation."
    )]
    pub async fn explain(
        &self,
        Parameters(request): Parameters<ExplainRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::explain::explain(self, request).await?,
        ))
    }

    /// Project architecture overview
    #[tool(
        description = "Get project architecture snapshot: layers, entry points, key types, and graph statistics. Use this first to understand a new codebase."
    )]
    pub async fn overview(
        &self,
        Parameters(request): Parameters<OverviewRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            super::overview::overview(self, request).await?,
        ))
    }
}
