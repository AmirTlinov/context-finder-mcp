//! MCP tool dispatch for Context
//!
//! Provides semantic code search capabilities to AI agents via MCP protocol.

use super::batch::{
    compute_used_chars, extract_path_from_input, parse_tool_result_as_json, prepare_item_input,
    push_item_or_truncate, resolve_batch_refs, trim_output_to_budget,
};
use super::catalog;
use super::cursor::{decode_cursor, encode_cursor, CURSOR_VERSION};
use super::evidence_fetch::compute_evidence_fetch_result;
use super::file_slice::compute_file_slice_result;
use super::grep_context::{compute_grep_context_result, GrepContextComputeOptions};
pub(super) use super::list_files::finalize_list_files_budget;
use super::list_files::{compute_list_files_result, decode_list_files_cursor};
use super::ls::{compute_ls_result, decode_ls_cursor, finalize_ls_budget};
use super::map::{compute_map_result, decode_map_cursor};
use super::meaning_focus::compute_meaning_focus_result;
use super::meaning_pack::compute_meaning_pack_result;
use super::notebook_apply_suggest::apply_notebook_apply_suggest;
use super::notebook_edit::apply_notebook_edit;
use super::notebook_pack::compute_notebook_pack_result;
use super::notebook_suggest::compute_notebook_suggest_result;
use super::paths::normalize_relative_path;
use super::repo_onboarding_pack::compute_repo_onboarding_pack_result;
use super::runbook_pack::compute_runbook_pack_result;
use super::schemas::atlas_pack::AtlasPackRequest;
use super::schemas::batch::{
    BatchBudget, BatchItemResult, BatchItemStatus, BatchRequest, BatchResult, BatchToolName,
};
use super::schemas::capabilities::CapabilitiesRequest;
use super::schemas::context::{ContextHit, ContextRequest, ContextResult, RelatedCode};
use super::schemas::context_pack::ContextPackRequest;
use super::schemas::doctor::{
    DoctorEnvResult, DoctorIndexDrift, DoctorIndexingObservability, DoctorObservability,
    DoctorProjectResult, DoctorRequest, DoctorResult, DoctorWarmIndexersObservability,
};
use super::schemas::evidence_fetch::EvidenceFetchRequest;
use super::schemas::explain::{ExplainRequest, ExplainResult};
use super::schemas::file_slice::{FileSliceCursorV1, FileSliceRequest};
use super::schemas::grep_context::{GrepContextCursorV1, GrepContextRequest};
use super::schemas::help::HelpRequest;
use super::schemas::impact::{ImpactRequest, ImpactResult, SymbolLocation, UsageInfo};
use super::schemas::list_files::ListFilesRequest;
#[cfg(test)]
use super::schemas::list_files::ListFilesTruncation;
use super::schemas::ls::LsRequest;
use super::schemas::map::MapRequest;
use super::schemas::meaning_focus::MeaningFocusRequest;
use super::schemas::meaning_pack::MeaningPackRequest;
use super::schemas::notebook_apply_suggest::NotebookApplySuggestRequest;
use super::schemas::notebook_edit::NotebookEditRequest;
use super::schemas::notebook_pack::NotebookPackRequest;
use super::schemas::notebook_suggest::NotebookSuggestRequest;
use super::schemas::overview::{
    GraphStats, KeyTypeInfo, LayerInfo, OverviewRequest, OverviewResult, ProjectInfo,
};
use super::schemas::read_pack::{
    ProjectFactsResult, ReadPackBudget, ReadPackIntent, ReadPackNextAction, ReadPackRecallResult,
    ReadPackRequest, ReadPackResult, ReadPackSection, ReadPackSnippet, ReadPackSnippetKind,
    ReadPackTruncation,
};
use super::schemas::repo_onboarding_pack::RepoOnboardingPackRequest;
use super::schemas::response_mode::ResponseMode;
use super::schemas::runbook_pack::RunbookPackRequest;
pub(super) use super::schemas::search::{SearchRequest, SearchResponse, SearchResult};
use super::schemas::text_search::{
    TextSearchCursorModeV1, TextSearchCursorV1, TextSearchMatch, TextSearchRequest,
    TextSearchResult,
};
use super::schemas::trace::{TraceRequest, TraceResult, TraceStep};
use super::schemas::worktree_pack::WorktreePackRequest;
use super::util::{path_has_extension_ignore_ascii_case, unix_ms};
use super::worktree_pack::{compute_worktree_pack_result, render_worktree_pack_block};
use anyhow::{Context as AnyhowContext, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use context_graph::{
    build_graph_docs, CodeGraph, ContextAssembler, GraphDocConfig, GraphEdge, GraphLanguage,
    GraphNode, RelationshipType, Symbol, GRAPH_DOC_VERSION,
};
use context_indexer::{
    assess_staleness, compute_project_watermark, read_index_watermark, root_fingerprint,
    FileScanner, IndexSnapshot, IndexState, IndexerError, PersistedIndexWatermark, ReindexAttempt,
    ReindexResult, ToolMeta, INDEX_STATE_SCHEMA_VERSION,
};
use context_protocol::{finalize_used_chars, BudgetTruncation};
use context_search::{
    ContextPackBudget, ContextPackItem, ContextPackOutput, MultiModelContextSearch,
    MultiModelHybridSearch, QueryClassifier, QueryType, SearchProfile, CONTEXT_PACK_VERSION,
};
use context_vector_store::{
    classify_path_kind, context_dir_for_project_root, corpus_path_for_project_root,
    current_model_id, ChunkCorpus, DocumentKind, GraphNodeDoc, GraphNodeStore, GraphNodeStoreMeta,
    QueryKind, VectorIndex, CONTEXT_DIR_NAME, LEGACY_CONTEXT_DIR_NAME,
};
use fs2::FileExt;
use getrandom::getrandom;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{Mutex, Notify};

mod budgets;
use budgets::{mcp_default_budgets, AutoIndexPolicy};

mod doctor_helpers;
use doctor_helpers::{
    load_corpus_chunk_ids, load_index_chunk_ids, load_model_statuses, sample_file_paths,
};

/// Context MCP Service
#[derive(Clone)]
pub struct ContextFinderService {
    /// Search profile
    profile: SearchProfile,
    /// Tool router
    tool_router: ToolRouter<Self>,
    /// Shared cache state (per-process)
    state: Arc<ServiceState>,
    /// Per-connection session defaults (do not share across multi-agent sessions).
    session: Arc<Mutex<SessionDefaults>>,
    /// Signals when `initialize -> roots/list` has either populated a session root or definitively
    /// finished (success or failure). Used to avoid racing the first tool call.
    roots_notify: Arc<Notify>,
    /// Whether to fall back to the server process cwd when no root hint is available.
    ///
    /// This is safe for in-process (per-agent) servers, but unsafe for the shared daemon
    /// where multiple projects share one backend.
    allow_cwd_root_fallback: bool,
}

impl ContextFinderService {
    pub fn new() -> Self {
        Self::new_with_policy(true)
    }

    pub fn new_daemon() -> Self {
        // Shared daemon mode: never guess a root from the daemon process cwd. Require either:
        // - explicit `path` on a tool call, or
        // - MCP roots capability (via initialize -> roots/list), or
        // - an explicit env override (CONTEXT_ROOT/CONTEXT_PROJECT_ROOT, legacy: CONTEXT_FINDER_ROOT).
        Self::new_with_policy(false)
    }

    fn new_with_policy(allow_cwd_root_fallback: bool) -> Self {
        Self {
            profile: load_profile_from_env(),
            tool_router: Self::tool_router(),
            state: Arc::new(ServiceState::new()),
            session: Arc::new(Mutex::new(SessionDefaults::default())),
            roots_notify: Arc::new(Notify::new()),
            allow_cwd_root_fallback,
        }
    }

    pub fn clone_for_connection(&self) -> Self {
        Self {
            profile: self.profile.clone(),
            tool_router: self.tool_router.clone(),
            state: self.state.clone(),
            session: Arc::new(Mutex::new(SessionDefaults::default())),
            roots_notify: Arc::new(Notify::new()),
            allow_cwd_root_fallback: self.allow_cwd_root_fallback,
        }
    }

    pub(super) async fn resolve_root(
        &self,
        raw_path: Option<&str>,
    ) -> Result<(PathBuf, String), String> {
        self.resolve_root_with_hints(raw_path, &[]).await
    }

    pub(super) async fn resolve_root_with_hints(
        &self,
        raw_path: Option<&str>,
        hints: &[String],
    ) -> Result<(PathBuf, String), String> {
        let (root, root_display) = self.resolve_root_impl_with_hints(raw_path, hints).await?;
        self.touch_daemon_best_effort(&root);
        Ok((root, root_display))
    }

    pub(super) async fn resolve_root_no_daemon_touch(
        &self,
        raw_path: Option<&str>,
    ) -> Result<(PathBuf, String), String> {
        self.resolve_root_with_hints_no_daemon_touch(raw_path, &[])
            .await
    }

    pub(super) async fn resolve_root_with_hints_no_daemon_touch(
        &self,
        raw_path: Option<&str>,
        hints: &[String],
    ) -> Result<(PathBuf, String), String> {
        self.resolve_root_impl_with_hints(raw_path, hints).await
    }

    async fn resolve_root_impl_with_hints(
        &self,
        raw_path: Option<&str>,
        hints: &[String],
    ) -> Result<(PathBuf, String), String> {
        if let Some(raw) = trimmed_non_empty(raw_path) {
            let root = canonicalize_root(raw).map_err(|err| format!("Invalid path: {err}"))?;
            let root_display = root.to_string_lossy().to_string();

            // Agent-native UX: callers often pass a "current file" path as `path`. Preserve the
            // relative file hint (when possible) so `read_pack intent=memory` can surface the
            // current working file without requiring extra parameters.
            let mut focus_file: Option<String> = None;
            if let Ok(canonical) = Path::new(raw).canonicalize() {
                if let Ok(meta) = std::fs::metadata(&canonical) {
                    if meta.is_file() {
                        if let Ok(rel) = canonical.strip_prefix(&root) {
                            focus_file = rel_path_string(rel);
                        }
                    }
                }
            }

            let mut session = self.session.lock().await;
            if self.allow_cwd_root_fallback || session.initialized {
                session.set_root(root.clone(), root_display.clone(), focus_file);
            }
            return Ok((root, root_display));
        }

        if let Some(root) = resolve_root_from_absolute_hints(hints) {
            let root_display = root.to_string_lossy().to_string();
            let mut session = self.session.lock().await;
            if self.allow_cwd_root_fallback || session.initialized {
                session.set_root(root.clone(), root_display.clone(), None);
            }
            return Ok((root, root_display));
        }

        let relative_hints = collect_relative_hints(hints);
        if self.allow_cwd_root_fallback && !relative_hints.is_empty() {
            if let Some((root, root_display)) =
                self.resolve_root_from_relative_hints(&relative_hints).await
            {
                return Ok((root, root_display));
            }
        }

        {
            let session = self.session.lock().await;
            if let Some((root, root_display)) = session.clone_root() {
                if self.allow_cwd_root_fallback || session.initialized {
                    return Ok((root, root_display));
                }
            }
        }

        // Race guard: MCP roots are populated asynchronously after initialize. Some clients send
        // the first tool call immediately after initialize, before `roots/list` completes.
        //
        // In shared daemon mode, failing fast can accidentally route the call using stale session
        // state (when a transport is reused), or force clients to redundantly pass `path` even when
        // they support roots. Prefer a small bounded wait to let `roots/list` establish the
        // per-connection session root.
        let roots_pending = { self.session.lock().await.roots_pending };
        if roots_pending {
            let wait_ms = if self.allow_cwd_root_fallback {
                150
            } else {
                900
            };
            let notify = self.roots_notify.clone();
            let _ = tokio::time::timeout(Duration::from_millis(wait_ms), notify.notified()).await;
            if let Some((root, root_display)) = self.session.lock().await.clone_root() {
                return Ok((root, root_display));
            }
        }

        if let Some((var, value)) = env_root_override() {
            let root = canonicalize_root(&value)
                .map_err(|err| format!("Invalid path from {var}: {err}"))?;
            let root_display = root.to_string_lossy().to_string();
            let mut session = self.session.lock().await;
            if self.allow_cwd_root_fallback || session.initialized {
                session.set_root(root.clone(), root_display.clone(), None);
            }
            return Ok((root, root_display));
        }

        if !self.allow_cwd_root_fallback {
            if self.session.lock().await.mcp_roots_ambiguous {
                return Err(
                    "Missing project root: multiple MCP workspace roots detected; pass `path` to disambiguate."
                        .to_string(),
                );
            }
            return Err(
                "Missing project root: pass `path` (recommended) or enable MCP roots, or set CONTEXT_ROOT/CONTEXT_PROJECT_ROOT."
                    .to_string(),
            );
        }

        let cwd = env::current_dir()
            .map_err(|err| format!("Failed to determine current directory: {err}"))?;
        let candidate = find_project_root(&cwd).unwrap_or(cwd);
        let root =
            canonicalize_root_path(&candidate).map_err(|err| format!("Invalid path: {err}"))?;
        let root_display = root.to_string_lossy().to_string();
        let mut session = self.session.lock().await;
        if self.allow_cwd_root_fallback || session.initialized {
            session.set_root(root.clone(), root_display.clone(), None);
        }
        Ok((root, root_display))
    }

    async fn resolve_root_from_relative_hints(
        &self,
        hints: &[String],
    ) -> Option<(PathBuf, String)> {
        let session_root = self.session.lock().await.clone_root();
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some((root, _)) = session_root.as_ref() {
            roots.push(root.clone());
        }
        for root in self.state.recent_roots().await {
            if !roots.iter().any(|known| known == &root) {
                roots.push(root);
            }
        }
        if roots.is_empty() {
            return None;
        }

        let mut best_score = 0usize;
        let mut best_roots: Vec<PathBuf> = Vec::new();
        for root in &roots {
            let score = hint_score_for_root(root, hints);
            if score == 0 {
                continue;
            }
            if score > best_score {
                best_score = score;
                best_roots.clear();
            }
            if score == best_score {
                best_roots.push(root.clone());
            }
        }

        if best_score == 0 || best_roots.is_empty() {
            return None;
        }

        let chosen = if best_roots.len() == 1 {
            best_roots.remove(0)
        } else if let Some((root, _)) = session_root {
            if best_roots.iter().any(|candidate| candidate == &root) {
                root
            } else {
                return None;
            }
        } else {
            return None;
        };

        let root_display = chosen.to_string_lossy().to_string();
        let mut session = self.session.lock().await;
        session.set_root(chosen.clone(), root_display.clone(), None);
        Some((chosen, root_display))
    }
}

fn load_profile_from_env() -> SearchProfile {
    let profile_name = std::env::var("CONTEXT_PROFILE")
        .or_else(|_| std::env::var("CONTEXT_FINDER_PROFILE"))
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "quality".to_string());

    if let Some(profile) = SearchProfile::builtin(&profile_name) {
        return profile;
    }

    let candidate_path = PathBuf::from(&profile_name);
    if candidate_path.exists() {
        match SearchProfile::from_file(&profile_name, &candidate_path) {
            Ok(profile) => return profile,
            Err(err) => {
                log::warn!(
                    "Failed to load profile from {}: {err:#}; falling back to builtin 'quality'",
                    candidate_path.display()
                );
            }
        }
    } else {
        log::warn!("Unknown profile '{profile_name}', falling back to builtin 'quality'");
    }

    SearchProfile::builtin("quality").unwrap_or_else(SearchProfile::general)
}

#[tool_handler]
impl ServerHandler for ContextFinderService {
    #[allow(clippy::manual_async_fn)]
    fn initialize(
        &self,
        request: rmcp::model::InitializeRequestParam,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<
        Output = std::result::Result<rmcp::model::InitializeResult, McpError>,
    > + Send
           + '_ {
        async move {
            // Treat every initialize as a fresh logical MCP session. Some MCP clients reuse a
            // long-lived server process (and/or transport) across multiple sessions, possibly in
            // different working directories. Without a reset, the daemon can retain a previous
            // session root and accidentally serve tool calls against the wrong project.
            {
                let mut session = self.session.lock().await;
                session.reset_for_initialize(request.capabilities.roots.is_some());
            }

            // Codex MCP client may be strict about the protocolVersion it requested during
            // initialization. rmcp defaults can lag behind, even when the tool surface is compatible.
            //
            // Agent-native behavior: echo the client's requested protocolVersion in the initialize
            // result so the transport stays open.
            if context.peer.peer_info().is_none() {
                context.peer.set_peer_info(request.clone());
            }

            // Session root: prefer the client's declared workspace roots when available.
            //
            // Important: do NOT block the initialize handshake on roots/list. Some MCP clients
            // cannot serve server->client requests until after initialization completes, and
            // blocking here can cause startup timeouts ("context deadline exceeded").
            if request.capabilities.roots.is_some() {
                let peer = context.peer.clone();
                let session = self.session.clone();
                let roots_notify = self.roots_notify.clone();
                tokio::spawn(async move {
                    // Give the client a moment to process the initialize response first.
                    tokio::time::sleep(Duration::from_millis(25)).await;

                    let roots = tokio::time::timeout(Duration::from_millis(800), peer.list_roots())
                        .await
                        .ok()
                        .and_then(|r| r.ok());

                    let mut candidates: Vec<PathBuf> = Vec::new();
                    if let Some(roots) = roots.as_ref() {
                        for root in &roots.roots {
                            let Some(path) = root_path_from_mcp_uri(&root.uri) else {
                                continue;
                            };
                            match canonicalize_root_path(&path) {
                                Ok(root) => candidates.push(root),
                                Err(err) => {
                                    log::debug!("Ignoring invalid MCP root {path:?}: {err}");
                                }
                            }
                        }
                    }
                    candidates.sort();
                    candidates.dedup();

                    let mut session = session.lock().await;
                    // Only set the session root if the tool call path did not already establish one.
                    // Explicit per-call `path` should win over workspace roots.
                    if session.root.is_none() {
                        match candidates.len() {
                            1 => {
                                let root = candidates.remove(0);
                                let root_display = root.to_string_lossy().to_string();
                                session.set_root(root, root_display, None);
                            }
                            n if n > 1 => {
                                // Fail-closed: do not guess a root when the workspace is multi-root.
                                // This prevents cross-project contamination in shared-backend mode.
                                session.mcp_roots_ambiguous = true;
                            }
                            _ => {}
                        }
                    }
                    session.roots_pending = false;
                    drop(session);
                    roots_notify.notify_waiters();
                });
            }

            let mut info = self.get_info();
            info.protocol_version = request.protocol_version;
            Ok(info)
        }
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(catalog::tool_instructions()),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            ..Default::default()
        }
    }
}

impl ContextFinderService {
    pub(super) async fn load_chunk_corpus(root: &Path) -> Result<Option<ChunkCorpus>> {
        let path = corpus_path_for_project_root(root);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(ChunkCorpus::load(&path).await.with_context(|| {
            format!("Failed to load chunk corpus {}", path.display())
        })?))
    }

    async fn tool_meta(&self, root: &Path) -> ToolMeta {
        if let Some(cached) = self.state.tool_meta_cache_get(root).await {
            return cached;
        }

        let root_display = root.to_string_lossy().to_string();
        let root_fp = root_fingerprint(&root_display);
        let meta = match gather_index_state(root, &self.profile).await {
            Ok(index_state) => ToolMeta {
                index_state: Some(index_state),
                root_fingerprint: Some(root_fp),
            },
            Err(err) => {
                log::debug!("index_state unavailable for {}: {err:#}", root.display());
                ToolMeta {
                    index_state: None,
                    root_fingerprint: Some(root_fp),
                }
            }
        };

        self.state.tool_meta_cache_put(root, meta.clone()).await;
        meta
    }

    async fn tool_meta_with_auto_index(&self, root: &Path, policy: AutoIndexPolicy) -> ToolMeta {
        let root_display = root.to_string_lossy().to_string();
        let root_fp = root_fingerprint(&root_display);
        let mut index_state = match gather_index_state(root, &self.profile).await {
            Ok(state) => state,
            Err(err) => {
                log::debug!("index_state unavailable for {}: {err:#}", root.display());
                return ToolMeta {
                    index_state: None,
                    root_fingerprint: Some(root_fp),
                };
            }
        };

        if policy.enabled && index_state.stale {
            // Agent-native performance: avoid long tail latencies on first use.
            //
            // Shared daemon mode (default): never block tool calls on indexing. If the index is
            // stale, request a background refresh and let semantic tools fall back until it
            // catches up.
            //
            // Daemon-disabled / stub environments: keep the previous behavior and attempt a
            // bounded inline build so semantic tools become usable without a manual `index` step.
            if !policy.allow_missing_index_rebuild {
                let reason = if index_state.stale_reasons.is_empty() {
                    "stale_index".to_string()
                } else {
                    format!("stale:{}", format_stale_reasons(&index_state.stale_reasons))
                };
                self.request_daemon_refresh_best_effort(root, &reason);
                index_state.reindex = Some(ReindexAttempt {
                    attempted: true,
                    performed: false,
                    budget_ms: Some(policy.budget_ms),
                    duration_ms: Some(0),
                    result: Some(ReindexResult::Skipped),
                    error: None,
                });
            } else {
                // If the index is missing, allow_missing_index_rebuild enables a bounded inline
                // build; if the index exists but is stale, attempt a bounded refresh so subsequent
                // calls stay fresh.
                let should_attempt = index_state.index.exists || policy.allow_missing_index_rebuild;
                let reindex = if should_attempt {
                    let mut budget_ms = policy.budget_ms;
                    if policy.budget_is_default {
                        if let Ok(Some(health)) = context_indexer::read_health_snapshot(root).await
                        {
                            if let Some(p95) = health.p95_duration_ms {
                                // Adaptive default: use recent p95 indexing time as a hint, without
                                // exploding tail latency (hard-capped globally).
                                let suggested = p95.saturating_mul(12).saturating_div(10);
                                budget_ms = budget_ms.max(suggested);
                            }
                        }
                    }
                    budget_ms = budgets::clamp_auto_index_budget_ms(budget_ms);

                    let reindex = self.attempt_reindex(root, budget_ms).await;
                    if let Ok(refreshed) = gather_index_state(root, &self.profile).await {
                        index_state = refreshed;
                    }
                    reindex
                } else {
                    ReindexAttempt {
                        attempted: true,
                        performed: false,
                        budget_ms: Some(policy.budget_ms),
                        duration_ms: Some(0),
                        result: Some(ReindexResult::Skipped),
                        error: None,
                    }
                };
                index_state.reindex = Some(reindex);
            }
        }

        let meta = ToolMeta {
            index_state: Some(index_state),
            root_fingerprint: Some(root_fp),
        };
        self.state.tool_meta_cache_put(root, meta.clone()).await;
        meta
    }

    async fn prepare_semantic_engine(
        &self,
        root: &Path,
        policy: AutoIndexPolicy,
    ) -> Result<(EngineLock, ToolMeta)> {
        // Reuse the cached index_state (`tool_meta`) to avoid repeating watermark scans on
        // back-to-back tool calls (especially expensive on non-git projects).
        let meta = if policy.enabled {
            self.tool_meta_with_auto_index(root, policy).await
        } else {
            self.tool_meta(root).await
        };

        let Some(index_state) = meta.index_state.clone() else {
            return Err(anyhow::anyhow!("index_state unavailable"));
        };

        if !index_state.index.exists {
            self.request_daemon_refresh_best_effort(root, "missing_index");
            return Err(anyhow::anyhow!(missing_index_message(
                &index_state,
                index_state.reindex.as_ref()
            )));
        }

        // Freshness-safe behavior: never serve silently stale semantic results. If the index is
        // stale (or became stale after a bounded refresh attempt), fall back to filesystem tools
        // while the daemon (or the next bounded refresh) catches up.
        if index_state.stale {
            if index_state.stale_reasons.is_empty() {
                self.request_daemon_refresh_best_effort(root, "stale_index");
            } else {
                let reason = format!("stale:{}", format_stale_reasons(&index_state.stale_reasons));
                self.request_daemon_refresh_best_effort(root, &reason);
            }
            return Err(anyhow::anyhow!(stale_index_message(
                &index_state,
                index_state.reindex.as_ref()
            )));
        }

        let engine = self.lock_engine(root).await?;
        Ok((engine, meta))
    }

    pub(in crate::tools::dispatch) async fn prepare_semantic_engine_for_query(
        &self,
        root: &Path,
        policy: AutoIndexPolicy,
        query: &str,
    ) -> Result<(EngineLock, ToolMeta)> {
        let (mut engine, meta) = self.prepare_semantic_engine(root, policy).await?;

        let Some(index_state) = meta.index_state.as_ref() else {
            return Ok((engine, meta));
        };

        let query_kind = match QueryClassifier::classify(query) {
            QueryType::Identifier => QueryKind::Identifier,
            QueryType::Path => QueryKind::Path,
            QueryType::Conceptual => QueryKind::Conceptual,
        };
        let desired_models = self.profile.experts().semantic_models(query_kind);
        if desired_models.is_empty() {
            return Ok((engine, meta));
        }

        let ensure = engine
            .engine_mut()
            .ensure_semantic_models_loaded(&index_state.project_watermark, desired_models)
            .await?;

        if !ensure.missing_models.is_empty() || !ensure.stale_models.is_empty() {
            // Best-effort: request a background refresh of missing/stale expert indices, but do not
            // hard-fail the tool. The search layer will fall back to available semantic sources,
            // keeping agents productive while the daemon catches up.
            let mut refresh_models = Vec::new();
            refresh_models.extend(ensure.missing_models.into_iter());
            refresh_models.extend(ensure.stale_models.into_iter());
            self.request_daemon_refresh_for_models_best_effort(
                root,
                "missing_or_stale_model_index",
                refresh_models,
            );
        }

        Ok((engine, meta))
    }

    async fn lock_engine(&self, root: &Path) -> Result<EngineLock> {
        self.touch_daemon_best_effort(root);

        let handle = self.state.engine_handle(root).await;
        let mut slot = handle.lock_owned().await;

        // Engine validity is tied to the canonical (primary) index and chunk corpus. Expert
        // indices are loaded/evicted independently and must not force full engine rebuilds.
        let primary_model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
        let signature = compute_engine_signature(root, &[primary_model_id]).await?;
        let needs_rebuild = slot
            .engine
            .as_ref()
            .is_none_or(|engine| engine.signature != signature);
        if needs_rebuild {
            slot.engine = None;
            slot.engine = Some(build_project_engine(root, &self.profile, signature).await?);
        }

        Ok(EngineLock { slot })
    }

    async fn maybe_warm_graph_nodes_store(&self, root: PathBuf, language: GraphLanguage) {
        let store_path = graph_nodes_store_path(&root);
        let key = store_path.to_string_lossy().to_string();

        if !self.state.graph_nodes_warmup_begin(key.clone()).await {
            return;
        }

        if tokio::runtime::Handle::try_current().is_err() {
            self.state.graph_nodes_warmup_finish(&key).await;
            return;
        }

        let service = self.clone();
        tokio::spawn(async move {
            if let Err(err) = warm_graph_nodes_store_task(service, root, language).await {
                log::debug!("graph_nodes warmup failed: {err:#}");
            }
        });
    }

    fn daemon_model_ids(&self) -> Vec<String> {
        // Agent-native default: keep indexes fresh without manual commands, while staying
        // resource-aware. Warm the primary model first, then (when resources permit) include the
        // profile's semantic expert models so retrieval quality doesn't silently degrade due to
        // drift in non-primary indices.
        let primary = current_model_id().unwrap_or_else(|_| "bge-small".to_string());

        let max_models = match total_memory_gib_linux_best_effort() {
            Some(mem_gib) if mem_gib <= 8 => 1,
            Some(mem_gib) if mem_gib <= 16 => 2,
            Some(mem_gib) if mem_gib <= 32 => 3,
            Some(_) => 4,
            None => 2,
        };

        let mut out: Vec<String> = Vec::new();
        out.push(primary.clone());

        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(primary);

        fn model_cost_rank(model_id: &str) -> u8 {
            let id = model_id.trim().to_ascii_lowercase();
            if id.contains("tiny") || id.contains("mini") || id.contains("small") {
                return 0;
            }
            if id.contains("base") {
                return 1;
            }
            if id.contains("large") || id.contains("xl") {
                return 2;
            }
            1
        }

        let experts = self.profile.experts();
        for kind in [
            QueryKind::Conceptual,
            QueryKind::Path,
            QueryKind::Identifier,
        ] {
            let mut models: Vec<&String> = experts.semantic_models(kind).iter().collect();
            models.sort_by(|a, b| {
                model_cost_rank(a)
                    .cmp(&model_cost_rank(b))
                    .then_with(|| a.cmp(b))
            });

            for model in models {
                if out.len() >= max_models {
                    break;
                }
                if seen.insert(model.clone()) {
                    out.push(model.clone());
                }
            }
            if out.len() >= max_models {
                break;
            }
        }

        out
    }

    fn touch_daemon_best_effort(&self, root: &Path) {
        let models = self.daemon_model_ids();
        self.touch_daemon_models_best_effort(root, models);
    }

    fn touch_daemon_models_best_effort(&self, root: &Path, models: Vec<String>) {
        let disable = std::env::var("CONTEXT_FINDER_DISABLE_DAEMON")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if disable {
            return;
        }

        // In stub embedding mode we aim for deterministic, dependency-light behavior
        // (used heavily in CI and tests). The background daemon is a performance
        // optimization and can introduce nondeterminism / flakiness in constrained
        // environments, so we skip it.
        let stub = std::env::var("CONTEXT_FINDER_EMBEDDING_MODE")
            .ok()
            .map(|v| v.trim().eq_ignore_ascii_case("stub"))
            .unwrap_or(false);
        if stub {
            return;
        }

        let root = root.to_path_buf();
        if tokio::runtime::Handle::try_current().is_err() {
            log::debug!("daemon touch skipped (no runtime)");
            return;
        }

        // Debounce daemon touches: many tools call `resolve_root` (and some call `lock_engine`),
        // so we avoid spawning a ping task for every single request.
        let profile = self.profile.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            if !state.should_touch_daemon(&root).await {
                return;
            }
            state.touch_warm_indexes(&root, &profile, models).await;
        });
    }

    fn request_daemon_refresh_best_effort(&self, root: &Path, reason: &str) {
        // Default refresh requests come from primary index staleness/missing checks. Keep them
        // cheap by targeting only the primary model; expert indices are refreshed on-demand
        // (query-kind aware) via `request_daemon_refresh_for_models_best_effort` below.
        let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
        self.request_daemon_refresh_for_models_best_effort(root, reason, vec![model_id]);
    }

    fn request_daemon_refresh_for_models_best_effort(
        &self,
        root: &Path,
        reason: &str,
        models: Vec<String>,
    ) {
        let disable = std::env::var("CONTEXT_FINDER_DISABLE_DAEMON")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if disable {
            return;
        }

        // In stub embedding mode we aim for deterministic, dependency-light behavior.
        // Background refresh is a performance optimization and can introduce nondeterminism,
        // so we skip it.
        let stub = std::env::var("CONTEXT_FINDER_EMBEDDING_MODE")
            .ok()
            .map(|v| v.trim().eq_ignore_ascii_case("stub"))
            .unwrap_or(false);
        if stub {
            return;
        }

        let root = root.to_path_buf();
        let reason = reason.to_string();
        if tokio::runtime::Handle::try_current().is_err() {
            log::debug!("daemon refresh skipped (no runtime)");
            return;
        }

        let profile = self.profile.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            // Always register the desired model roster with the warm-indexer, even if the refresh
            // trigger itself is debounced. This prevents "missing expert model index" scenarios
            // from being silently delayed by a recent refresh for another model set.
            state
                .touch_warm_indexes(&root, &profile, models.clone())
                .await;

            if state.should_request_daemon_refresh(&root).await {
                state
                    .request_warm_indexes_refresh(&root, &profile, models, &reason)
                    .await;
            }
        });
    }

    async fn attempt_reindex(&self, root: &Path, budget_ms: u64) -> ReindexAttempt {
        let start = Instant::now();
        let mut attempt = ReindexAttempt {
            attempted: true,
            performed: false,
            budget_ms: Some(budget_ms),
            duration_ms: None,
            result: None,
            error: None,
        };

        let templates = self.profile.embedding().clone();
        let indexer =
            match context_indexer::ProjectIndexer::new_with_embedding_templates(root, templates)
                .await
            {
                Ok(indexer) => indexer,
                Err(err) => {
                    attempt.duration_ms = Some(start.elapsed().as_millis() as u64);
                    attempt.result = Some(ReindexResult::Failed);
                    attempt.error = Some(err.to_string());
                    return attempt;
                }
            };

        match indexer
            .index_with_budget(Duration::from_millis(budget_ms))
            .await
        {
            Ok(_) => {
                attempt.performed = true;
                attempt.result = Some(ReindexResult::Ok);
            }
            Err(IndexerError::BudgetExceeded) => {
                attempt.result = Some(ReindexResult::BudgetExceeded);
            }
            Err(err) => {
                attempt.result = Some(ReindexResult::Failed);
                attempt.error = Some(err.to_string());
            }
        }

        attempt.duration_ms = Some(start.elapsed().as_millis() as u64);
        attempt
    }
}

async fn warm_graph_nodes_store_task(
    service: ContextFinderService,
    root: PathBuf,
    language: GraphLanguage,
) -> Result<()> {
    let store_path = graph_nodes_store_path(&root);
    let key = store_path.to_string_lossy().to_string();

    let outcome = async {
        let mut engine = service.lock_engine(&root).await?;
        engine.engine_mut().ensure_graph(language).await?;

        let graph_nodes_cfg = service.profile.graph_nodes();
        let canonical_index_mtime = engine.engine_mut().canonical_index_mtime;
        let source_index_mtime_ms = unix_ms(canonical_index_mtime);
        let Some(assembler) = engine.engine_mut().context_search.assembler() else {
            return Ok(());
        };

        let language_key = graph_language_key(language).to_string();
        let template_hash = service.profile.embedding().graph_node_template_hash();

        let loaded = GraphNodeStore::load(&store_path)
            .await
            .map_or(None, |store| {
                let meta = store.meta();
                (meta.source_index_mtime_ms == source_index_mtime_ms
                    && meta.graph_language == language_key
                    && meta.graph_doc_version == GRAPH_DOC_VERSION
                    && meta.template_hash == template_hash)
                    .then_some(store)
            });

        if loaded.is_some() {
            return Ok(());
        }

        let docs = build_graph_docs(
            assembler,
            GraphDocConfig {
                max_neighbors_per_relation: graph_nodes_cfg.max_neighbors_per_relation,
            },
        );
        let docs: Vec<GraphNodeDoc> = docs
            .into_iter()
            .map(|doc| {
                let text = service
                    .profile
                    .embedding()
                    .render_graph_node_doc(&doc.doc)
                    .unwrap_or(doc.doc);
                GraphNodeDoc {
                    node_id: doc.node_id,
                    chunk_id: doc.chunk_id,
                    text,
                    doc_hash: doc.doc_hash,
                }
            })
            .collect();

        drop(engine);

        let meta = GraphNodeStoreMeta::for_current_model(
            source_index_mtime_ms,
            language_key,
            GRAPH_DOC_VERSION,
            template_hash,
        )?;
        GraphNodeStore::build_or_update(&store_path, meta, docs).await?;
        Ok(())
    }
    .await;

    service.state.graph_nodes_warmup_finish(&key).await;
    outcome
}

fn model_id_dir_name(model_id: &str) -> String {
    model_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

fn graph_nodes_store_path(root: &Path) -> PathBuf {
    let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    context_dir_for_project_root(root)
        .join("indexes")
        .join(model_id_dir_name(&model_id))
        .join("graph_nodes.json")
}

fn index_path_for_model(root: &Path, model_id: &str) -> PathBuf {
    context_dir_for_project_root(root)
        .join("indexes")
        .join(model_id_dir_name(model_id))
        .join("index.json")
}

async fn load_store_mtime(path: &Path) -> Result<SystemTime> {
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("Failed to read metadata for {}", path.display()))?;
    metadata
        .modified()
        .with_context(|| format!("Failed to read modification time for {}", path.display()))
}

async fn gather_index_state(root: &Path, profile: &SearchProfile) -> Result<IndexState> {
    let project_watermark = compute_project_watermark(root).await?;
    let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    let store_path = index_path_for_model(root, &model_id);
    let index_exists = store_path.exists();

    let mut index_corrupt = false;
    let mut index_mtime_ms = None;
    if index_exists {
        match load_store_mtime(&store_path).await {
            Ok(mtime) => {
                index_mtime_ms = Some(unix_ms(mtime));
            }
            Err(_) => {
                index_corrupt = true;
            }
        }
    }

    let mut watermark = None;
    let mut built_at_unix_ms = None;
    match read_index_watermark(&store_path).await {
        Ok(Some(PersistedIndexWatermark {
            built_at_unix_ms: built_at,
            watermark: mark,
        })) => {
            built_at_unix_ms = Some(built_at);
            watermark = Some(mark);
        }
        Ok(None) => {}
        Err(_) => {
            index_corrupt = true;
        }
    }

    let assessment = assess_staleness(
        &project_watermark,
        index_exists,
        index_corrupt,
        watermark.as_ref(),
    );

    let snapshot = IndexSnapshot {
        exists: index_exists,
        path: Some(store_path.display().to_string()),
        mtime_ms: index_mtime_ms,
        built_at_unix_ms,
        watermark,
    };

    Ok(IndexState {
        schema_version: INDEX_STATE_SCHEMA_VERSION,
        project_root: Some(root.display().to_string()),
        model_id,
        profile: profile.name().to_string(),
        project_watermark,
        index: snapshot,
        stale: assessment.stale,
        stale_reasons: assessment.reasons,
        reindex: None,
    })
}

fn missing_index_message(state: &IndexState, attempt: Option<&ReindexAttempt>) -> String {
    let path = state
        .index
        .path
        .as_deref()
        .unwrap_or("<unknown-index-path>");
    let mut message =
        format!("Index not found at {path}. Auto-index is starting in the background.");
    if let Some(attempt) = attempt {
        message.push_str(" Auto-index attempt: ");
        message.push_str(&format_reindex_attempt(attempt));
        message.push('.');
    }
    message
}

fn stale_index_message(state: &IndexState, attempt: Option<&ReindexAttempt>) -> String {
    let mut message =
        "Index is stale. Semantic tools will fall back to filesystem results while it refreshes."
            .to_string();
    if let Some(attempt) = attempt {
        message.push_str(" Auto-index attempt: ");
        message.push_str(&format_reindex_attempt(attempt));
        message.push('.');
    }
    if !state.stale_reasons.is_empty() {
        message.push_str(" Stale reasons: ");
        message.push_str(&format_stale_reasons(&state.stale_reasons));
        message.push('.');
    }
    message
}

fn stale_reason_name(reason: &context_indexer::StaleReason) -> &'static str {
    match reason {
        context_indexer::StaleReason::IndexMissing => "index_missing",
        context_indexer::StaleReason::IndexCorrupt => "index_corrupt",
        context_indexer::StaleReason::WatermarkMissing => "watermark_missing",
        context_indexer::StaleReason::GitHeadMismatch => "git_head_mismatch",
        context_indexer::StaleReason::GitDirtyMismatch => "git_dirty_mismatch",
        context_indexer::StaleReason::FilesystemChanged => "filesystem_changed",
    }
}

fn format_stale_reasons(reasons: &[context_indexer::StaleReason]) -> String {
    if reasons.is_empty() {
        return "unknown".to_string();
    }
    reasons
        .iter()
        .map(stale_reason_name)
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_reindex_attempt(attempt: &ReindexAttempt) -> String {
    let budget = attempt
        .budget_ms
        .map(|v| format!("{v}ms"))
        .unwrap_or_else(|| "unknown".to_string());
    let duration = attempt
        .duration_ms
        .map(|v| format!("{v}ms"))
        .unwrap_or_else(|| "unknown".to_string());

    match attempt.result {
        Some(ReindexResult::Ok) => format!("ok in {duration} (budget {budget})"),
        Some(ReindexResult::BudgetExceeded) => {
            format!("budget exceeded (ran {duration}, budget {budget})")
        }
        Some(ReindexResult::Failed) => format!(
            "failed in {duration} (budget {budget}): {}",
            attempt.error.as_deref().unwrap_or("unknown error")
        ),
        Some(ReindexResult::Skipped) | None => "skipped".to_string(),
    }
}

async fn load_semantic_indexes(root: &Path) -> Result<Vec<(String, VectorIndex)>> {
    // Low-RAM default: only load the primary semantic index. Expert indices are loaded on-demand
    // per-query (and evicted by an engine-local LRU) to avoid ballooning RSS in a shared daemon.
    let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    let path = index_path_for_model(root, &model_id);
    anyhow::ensure!(
        path.exists(),
        "No semantic indices available (missing {})",
        path.display()
    );
    let index = VectorIndex::load(&path)
        .await
        .with_context(|| format!("Failed to load index {}", path.display()))?;
    Ok(vec![(model_id, index)])
}

fn build_chunk_lookup(chunks: &[context_code_chunker::CodeChunk]) -> HashMap<String, usize> {
    let mut lookup = HashMap::new();
    for (idx, chunk) in chunks.iter().enumerate() {
        lookup.insert(
            format!(
                "{}:{}:{}",
                chunk.file_path, chunk.start_line, chunk.end_line
            ),
            idx,
        );
    }
    lookup
}

// ============================================================================
// MCP Engine Cache (per-project, long-lived)
// ============================================================================

const ENGINE_CACHE_CAPACITY: usize = 4;
const TOOL_META_CACHE_CAPACITY: usize = 8;
const TOOL_META_CACHE_TTL: Duration = Duration::from_secs(10);
const TOOL_META_CACHE_GIT_WATERMARK_MAX_AGE_MS: u64 = 1_500;
const TOOL_META_CACHE_FS_WATERMARK_MAX_AGE_MS: u64 = 10_000;
const PROJECT_FACTS_CACHE_CAPACITY: usize = 8;
const PROJECT_FACTS_CACHE_TTL: Duration = Duration::from_secs(10);
const CURSOR_STORE_CAPACITY: usize = 256;
const CURSOR_STORE_TTL: Duration = Duration::from_secs(60 * 60);

fn total_memory_gib_linux_best_effort() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in contents.lines() {
        let line = line.trim_start();
        if !line.starts_with("MemTotal:") {
            continue;
        }
        let kb = line
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse::<u64>().ok())?;
        return Some(kb / 1024 / 1024);
    }
    None
}

fn default_engine_semantic_index_cache_capacity() -> usize {
    let Some(mem_gib) = total_memory_gib_linux_best_effort() else {
        return 3;
    };

    if mem_gib <= 8 {
        1
    } else if mem_gib <= 16 {
        2
    } else {
        3
    }
}

fn engine_semantic_index_cache_capacity_from_env() -> usize {
    std::env::var("CONTEXT_FINDER_ENGINE_SEMANTIC_INDEX_CAPACITY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(default_engine_semantic_index_cache_capacity)
        .max(1)
}

// Daemon touch is a best-effort background performance optimization (keeps incremental indexes
// warm). Avoid spawning a new ping task on every tool call: shared backends can be chatty.
const DAEMON_TOUCH_CACHE_CAPACITY: usize = 32;
const DAEMON_TOUCH_DEBOUNCE: Duration = Duration::from_millis(750);
const DAEMON_REFRESH_CACHE_CAPACITY: usize = 32;
const DAEMON_REFRESH_DEBOUNCE: Duration = Duration::from_millis(750);

type EngineHandle = Arc<Mutex<EngineSlot>>;

struct ServiceState {
    engines: Mutex<EngineCache>,
    tool_meta_cache: Mutex<ToolMetaCache>,
    project_facts_cache: Mutex<ProjectFactsCache>,
    cursor_store: Mutex<CursorStore>,
    graph_nodes_warmup: Mutex<GraphNodesWarmupState>,
    daemon_touch_cache: Mutex<DaemonTouchCache>,
    daemon_refresh_cache: Mutex<DaemonTouchCache>,
    warm_indexes: Mutex<crate::index_warmup::WarmIndexers>,
}

impl ServiceState {
    fn new() -> Self {
        Self {
            engines: Mutex::new(EngineCache::new(ENGINE_CACHE_CAPACITY)),
            tool_meta_cache: Mutex::new(ToolMetaCache::new()),
            project_facts_cache: Mutex::new(ProjectFactsCache::new()),
            cursor_store: Mutex::new(CursorStore::new()),
            graph_nodes_warmup: Mutex::new(GraphNodesWarmupState::new()),
            daemon_touch_cache: Mutex::new(DaemonTouchCache::new()),
            daemon_refresh_cache: Mutex::new(DaemonTouchCache::new()),
            warm_indexes: Mutex::new(crate::index_warmup::WarmIndexers::new()),
        }
    }

    async fn engine_handle(&self, root: &Path) -> EngineHandle {
        let mut cache = self.engines.lock().await;
        cache.get_or_insert(root)
    }

    async fn tool_meta_cache_get(&self, root: &Path) -> Option<ToolMeta> {
        let cached = {
            let mut cache = self.tool_meta_cache.lock().await;
            cache.get(root)
        }?;

        let Some(index_state) = cached.index_state.as_ref() else {
            return Some(cached);
        };

        // Freshness-safe behavior: cached index_state must not allow silently stale semantic
        // results. We cache to avoid repeated watermark scans, but we still re-check periodically
        // (tuned per watermark kind).
        let computed_at_ms = match &index_state.project_watermark {
            context_indexer::Watermark::Git {
                computed_at_unix_ms,
                ..
            } => computed_at_unix_ms,
            context_indexer::Watermark::Filesystem {
                computed_at_unix_ms,
                ..
            } => computed_at_unix_ms,
        };
        if let Some(computed_at_ms) = computed_at_ms {
            let now_ms = unix_ms(std::time::SystemTime::now());
            let max_age_ms = match &index_state.project_watermark {
                context_indexer::Watermark::Git { .. } => TOOL_META_CACHE_GIT_WATERMARK_MAX_AGE_MS,
                context_indexer::Watermark::Filesystem { .. } => {
                    TOOL_META_CACHE_FS_WATERMARK_MAX_AGE_MS
                }
            };
            if now_ms.saturating_sub(*computed_at_ms) > max_age_ms {
                let mut cache = self.tool_meta_cache.lock().await;
                cache.remove(root);
                return None;
            }
        }

        if !index_state.stale {
            return Some(cached);
        }

        // Fast freshness recovery: stale meta is cached to avoid expensive watermark scans, but if
        // the index file changes (e.g., daemon refresh completed), the cached stale state becomes
        // obsolete. Do a cheap index file stat and evict on change so the next call re-gathers.
        let store_path = index_path_for_model(root, &index_state.model_id);
        let should_evict = if !index_state.index.exists {
            store_path.exists()
        } else if let Some(cached_mtime) = index_state.index.mtime_ms {
            match load_store_mtime(&store_path).await {
                Ok(mtime) => unix_ms(mtime) != cached_mtime,
                Err(_) => true,
            }
        } else {
            // We don't have a stored mtime but the index exists; if the file can be stat'ed,
            // treat it as a change signal and force a refresh.
            load_store_mtime(&store_path).await.is_ok()
        };

        if should_evict {
            let mut cache = self.tool_meta_cache.lock().await;
            cache.remove(root);
            return None;
        }

        Some(cached)
    }

    async fn tool_meta_cache_put(&self, root: &Path, meta: ToolMeta) {
        let mut cache = self.tool_meta_cache.lock().await;
        cache.insert(root, meta);
    }

    async fn project_facts_cache_get(&self, root: &Path) -> Option<ProjectFactsResult> {
        let mut cache = self.project_facts_cache.lock().await;
        cache.get(root)
    }

    async fn project_facts_cache_put(&self, root: &Path, facts: ProjectFactsResult) {
        let mut cache = self.project_facts_cache.lock().await;
        cache.insert(root, facts);
    }

    async fn cursor_store_put(&self, payload: Vec<u8>) -> u64 {
        let mut store = self.cursor_store.lock().await;
        store.insert_persisted_best_effort(payload)
    }

    async fn cursor_store_get(&self, id: u64) -> Option<Vec<u8>> {
        let mut store = self.cursor_store.lock().await;
        store.get(id)
    }

    async fn graph_nodes_warmup_begin(&self, key: String) -> bool {
        let mut warmup = self.graph_nodes_warmup.lock().await;
        warmup.begin(key)
    }

    async fn graph_nodes_warmup_finish(&self, key: &str) {
        let mut warmup = self.graph_nodes_warmup.lock().await;
        warmup.finish(key);
    }

    async fn should_touch_daemon(&self, root: &Path) -> bool {
        let mut cache = self.daemon_touch_cache.lock().await;
        cache.should_touch(root, Instant::now())
    }

    async fn should_request_daemon_refresh(&self, root: &Path) -> bool {
        let mut cache = self.daemon_refresh_cache.lock().await;
        cache.should_touch_with(
            root,
            Instant::now(),
            DAEMON_REFRESH_DEBOUNCE,
            DAEMON_REFRESH_CACHE_CAPACITY,
        )
    }

    async fn touch_warm_indexes(
        &self,
        root: &Path,
        profile: &SearchProfile,
        model_ids: Vec<String>,
    ) {
        let mut warm = self.warm_indexes.lock().await;
        warm.touch(root, profile, model_ids).await;
    }

    async fn request_warm_indexes_refresh(
        &self,
        root: &Path,
        profile: &SearchProfile,
        model_ids: Vec<String>,
        reason: &str,
    ) {
        let mut warm = self.warm_indexes.lock().await;
        warm.touch(root, profile, model_ids.clone()).await;
        warm.request_refresh(root, reason, model_ids).await;
    }

    async fn recent_roots(&self) -> Vec<PathBuf> {
        let guard = self.engines.lock().await;
        guard.recent_roots()
    }
}

struct GraphNodesWarmupState {
    in_flight: HashSet<String>,
}

struct DaemonTouchCache {
    entries: HashMap<PathBuf, Instant>,
    order: VecDeque<PathBuf>,
}

impl DaemonTouchCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn should_touch(&mut self, root: &Path, now: Instant) -> bool {
        self.should_touch_with(
            root,
            now,
            DAEMON_TOUCH_DEBOUNCE,
            DAEMON_TOUCH_CACHE_CAPACITY,
        )
    }

    fn should_touch_with(
        &mut self,
        root: &Path,
        now: Instant,
        debounce: Duration,
        capacity: usize,
    ) -> bool {
        let key = root.to_path_buf();
        if let Some(last) = self.entries.get(&key).copied() {
            if now.duration_since(last) < debounce {
                return false;
            }
        }

        self.entries.insert(key.clone(), now);
        self.order.retain(|k| k != &key);
        self.order.push_back(key);

        while self.order.len() > capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.entries.remove(&evicted);
            }
        }

        true
    }
}

impl GraphNodesWarmupState {
    fn new() -> Self {
        Self {
            in_flight: HashSet::new(),
        }
    }

    fn begin(&mut self, key: String) -> bool {
        self.in_flight.insert(key)
    }

    fn finish(&mut self, key: &str) {
        self.in_flight.remove(key);
    }
}

#[derive(Clone)]
struct ToolMetaCacheEntry {
    meta: ToolMeta,
    expires_at: Instant,
}

struct ToolMetaCache {
    entries: HashMap<PathBuf, ToolMetaCacheEntry>,
    order: VecDeque<PathBuf>,
}

impl ToolMetaCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, root: &Path) -> Option<ToolMeta> {
        let now = Instant::now();
        self.prune_expired(now);

        let key = root.to_path_buf();
        let entry = self.entries.remove(&key)?;
        if entry.expires_at <= now {
            self.order.retain(|k| k != &key);
            return None;
        }

        self.order.retain(|k| k != &key);
        self.order.push_back(key.clone());
        let meta = entry.meta.clone();
        self.entries.insert(key, entry);
        Some(meta)
    }

    fn remove(&mut self, root: &Path) {
        let key = root.to_path_buf();
        self.entries.remove(&key);
        self.order.retain(|k| k != &key);
    }

    fn insert(&mut self, root: &Path, meta: ToolMeta) {
        let now = Instant::now();
        self.prune_expired(now);

        let key = root.to_path_buf();
        let entry = ToolMetaCacheEntry {
            meta,
            expires_at: now + TOOL_META_CACHE_TTL,
        };

        self.entries.insert(key.clone(), entry);
        self.order.retain(|k| k != &key);
        self.order.push_back(key);

        while self.order.len() > TOOL_META_CACHE_CAPACITY {
            if let Some(evicted) = self.order.pop_front() {
                self.entries.remove(&evicted);
            }
        }
    }

    fn prune_expired(&mut self, now: Instant) {
        let mut expired_keys: Vec<PathBuf> = Vec::new();
        for (key, entry) in &self.entries {
            if entry.expires_at <= now {
                expired_keys.push(key.clone());
            }
        }

        for key in expired_keys {
            self.entries.remove(&key);
        }

        self.order.retain(|key| self.entries.contains_key(key));
    }
}

#[cfg(test)]
mod meta_cache_tests {
    use super::*;
    use tempfile::tempdir;

    fn minimal_index_state(
        root: &Path,
        model_id: &str,
        project_watermark: context_indexer::Watermark,
        index_exists: bool,
        index_mtime_ms: Option<u64>,
        stale: bool,
    ) -> IndexState {
        IndexState {
            schema_version: INDEX_STATE_SCHEMA_VERSION,
            project_root: Some(root.to_string_lossy().to_string()),
            model_id: model_id.to_string(),
            profile: "quality".to_string(),
            project_watermark,
            index: IndexSnapshot {
                exists: index_exists,
                path: Some(
                    index_path_for_model(root, model_id)
                        .to_string_lossy()
                        .to_string(),
                ),
                mtime_ms: index_mtime_ms,
                built_at_unix_ms: None,
                watermark: None,
            },
            stale,
            stale_reasons: Vec::new(),
            reindex: None,
        }
    }

    #[tokio::test]
    async fn evicts_cached_meta_when_project_watermark_is_too_old() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();
        let now_ms = unix_ms(std::time::SystemTime::now());

        let stale_age_ms = TOOL_META_CACHE_GIT_WATERMARK_MAX_AGE_MS.saturating_add(1);
        let watermark = context_indexer::Watermark::Git {
            computed_at_unix_ms: Some(now_ms.saturating_sub(stale_age_ms)),
            git_head: "deadbeef".to_string(),
            git_dirty: false,
            dirty_hash: None,
        };

        let meta = ToolMeta {
            index_state: Some(minimal_index_state(
                root,
                "bge-small",
                watermark,
                false,
                None,
                false,
            )),
            root_fingerprint: Some(1),
        };

        let state = ServiceState::new();
        state.tool_meta_cache_put(root, meta).await;

        let cached = state.tool_meta_cache_get(root).await;
        assert!(
            cached.is_none(),
            "expected cache eviction when watermark is too old"
        );
    }

    #[tokio::test]
    async fn evicts_cached_stale_meta_when_index_mtime_changes() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();

        // Seed an index file so we can observe mtime changes cheaply.
        let store_path = index_path_for_model(root, "bge-small");
        tokio::fs::create_dir_all(store_path.parent().expect("parent"))
            .await
            .expect("mkdir index dir");
        tokio::fs::write(&store_path, b"{\"schema_version\":3}")
            .await
            .expect("write index");

        let initial_mtime_ms = unix_ms(load_store_mtime(&store_path).await.expect("mtime"));
        let watermark = context_indexer::Watermark::Filesystem {
            computed_at_unix_ms: Some(unix_ms(std::time::SystemTime::now())),
            file_count: 1,
            max_mtime_ms: 1,
            total_bytes: 1,
        };

        let meta = ToolMeta {
            index_state: Some(minimal_index_state(
                root,
                "bge-small",
                watermark,
                true,
                Some(initial_mtime_ms),
                true,
            )),
            root_fingerprint: Some(1),
        };

        let state = ServiceState::new();
        state.tool_meta_cache_put(root, meta).await;

        // Ensure mtime changes (some filesystems have coarse resolution).
        let mut updated_mtime_ms = initial_mtime_ms;
        for attempt in 0..64u64 {
            tokio::fs::write(&store_path, format!("{{\"attempt\":{attempt}}}"))
                .await
                .expect("rewrite index");
            updated_mtime_ms = unix_ms(load_store_mtime(&store_path).await.expect("mtime"));
            if updated_mtime_ms != initial_mtime_ms {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_ne!(
            updated_mtime_ms, initial_mtime_ms,
            "expected index mtime to change for the test"
        );

        let cached = state.tool_meta_cache_get(root).await;
        assert!(
            cached.is_none(),
            "expected cache eviction when index mtime changed"
        );
    }
}

#[derive(Clone)]
struct ProjectFactsCacheEntry {
    facts: ProjectFactsResult,
    expires_at: Instant,
}

struct ProjectFactsCache {
    entries: HashMap<PathBuf, ProjectFactsCacheEntry>,
    order: VecDeque<PathBuf>,
}

impl ProjectFactsCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, root: &Path) -> Option<ProjectFactsResult> {
        let now = Instant::now();
        self.prune_expired(now);

        let key = root.to_path_buf();
        let entry = self.entries.remove(&key)?;
        if entry.expires_at <= now {
            self.order.retain(|k| k != &key);
            return None;
        }

        self.order.retain(|k| k != &key);
        self.order.push_back(key.clone());
        let facts = entry.facts.clone();
        self.entries.insert(key, entry);
        Some(facts)
    }

    fn insert(&mut self, root: &Path, facts: ProjectFactsResult) {
        let now = Instant::now();
        self.prune_expired(now);

        let key = root.to_path_buf();
        let entry = ProjectFactsCacheEntry {
            facts,
            expires_at: now + PROJECT_FACTS_CACHE_TTL,
        };

        self.entries.insert(key.clone(), entry);
        self.order.retain(|k| k != &key);
        self.order.push_back(key);

        while self.order.len() > PROJECT_FACTS_CACHE_CAPACITY {
            if let Some(evicted) = self.order.pop_front() {
                self.entries.remove(&evicted);
            }
        }
    }

    fn prune_expired(&mut self, now: Instant) {
        let mut expired_keys: Vec<PathBuf> = Vec::new();
        for (key, entry) in &self.entries {
            if entry.expires_at <= now {
                expired_keys.push(key.clone());
            }
        }

        for key in expired_keys {
            self.entries.remove(&key);
        }

        self.order.retain(|key| self.entries.contains_key(key));
    }
}

#[derive(Clone)]
struct CursorStoreEntry {
    payload: Vec<u8>,
    expires_at: Instant,
}

#[derive(Clone)]
struct PersistedCursorStoreEntryData {
    payload: Vec<u8>,
    expires_at_unix_ms: u64,
}

struct CursorStore {
    next_id: u64,
    entries: HashMap<u64, CursorStoreEntry>,
    order: VecDeque<u64>,
    persist_path: Option<PathBuf>,
}

impl CursorStore {
    fn random_u64_best_effort() -> Option<u64> {
        let mut bytes = [0u8; 8];
        getrandom(&mut bytes).ok()?;
        Some(u64::from_be_bytes(bytes).max(1))
    }

    fn new() -> Self {
        let seed = Self::random_u64_best_effort().unwrap_or(1).max(1);
        let mut store = Self {
            next_id: seed,
            entries: HashMap::new(),
            order: VecDeque::new(),
            persist_path: cursor_store_persist_path(),
        };
        store.load_best_effort();
        store
    }

    fn get(&mut self, id: u64) -> Option<Vec<u8>> {
        let now = Instant::now();
        self.prune_expired(now);

        let entry = self.entries.remove(&id)?;
        if entry.expires_at <= now {
            self.order.retain(|k| k != &id);
            return None;
        }

        self.order.retain(|k| k != &id);
        self.order.push_back(id);
        let payload = entry.payload.clone();
        self.entries.insert(id, entry);
        Some(payload)
    }

    fn insert_persisted_best_effort(&mut self, payload: Vec<u8>) -> u64 {
        let Some(path) = self.persist_path.clone() else {
            return self.insert(payload);
        };

        let Some(_lock) = Self::acquire_persist_lock_best_effort(&path) else {
            // If we cannot safely persist shared cursor aliases, prefer an in-memory-only insert.
            // This avoids cross-process collisions at the cost of losing persistence.
            return self.insert(payload);
        };

        let now_instant = Instant::now();
        self.prune_expired(now_instant);

        let now_unix_ms = unix_ms(SystemTime::now());
        let (mut order, mut entries, disk_max_id) =
            Self::load_persisted_best_effort(&path, now_unix_ms);

        // Allocate an ID under the persistence lock to avoid collisions across processes.
        // Prefer random IDs: compact cursors are frequently copy-pasted across sessions, so
        // predictable low IDs (1,2,3,...) increase the chance that a cursor token accidentally
        // resolves to the wrong continuation in a different process.
        let mut id: Option<u64> = None;
        for _ in 0..8 {
            let Some(candidate) = Self::random_u64_best_effort() else {
                break;
            };
            if !entries.contains_key(&candidate) && !self.entries.contains_key(&candidate) {
                id = Some(candidate);
                break;
            }
        }
        let mut id = id.unwrap_or_else(|| {
            let mut candidate = self.next_id.max(disk_max_id.wrapping_add(1).max(1)).max(1);
            while entries.contains_key(&candidate) || self.entries.contains_key(&candidate) {
                candidate = candidate.wrapping_add(1).max(1);
            }
            candidate
        });
        while entries.contains_key(&id) || self.entries.contains_key(&id) {
            id = id.wrapping_add(1).max(1);
        }
        self.next_id = id.wrapping_add(1).max(1);

        self.insert_entry(
            id,
            CursorStoreEntry {
                payload,
                expires_at: now_instant + CURSOR_STORE_TTL,
            },
        );

        // Merge in-memory entries into the persisted view so we don't clobber other processes'
        // continuations when writing the file.
        for mem_id in &self.order {
            let Some(entry) = self.entries.get(mem_id) else {
                continue;
            };
            let remaining = entry.expires_at.saturating_duration_since(now_instant);
            let expires_at_unix_ms = now_unix_ms
                .saturating_add(u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX));
            entries.insert(
                *mem_id,
                PersistedCursorStoreEntryData {
                    payload: entry.payload.clone(),
                    expires_at_unix_ms,
                },
            );
            order.retain(|k| k != mem_id);
            order.push_back(*mem_id);
        }

        while order.len() > CURSOR_STORE_CAPACITY {
            if let Some(evicted) = order.pop_front() {
                entries.remove(&evicted);
            }
        }

        Self::persist_persisted_best_effort(&path, &order, &entries);

        id
    }

    fn insert_entry(&mut self, id: u64, entry: CursorStoreEntry) {
        self.entries.insert(id, entry);
        self.order.retain(|k| k != &id);
        self.order.push_back(id);

        while self.order.len() > CURSOR_STORE_CAPACITY {
            if let Some(evicted) = self.order.pop_front() {
                self.entries.remove(&evicted);
            }
        }
    }

    fn insert(&mut self, payload: Vec<u8>) -> u64 {
        let now = Instant::now();
        self.prune_expired(now);

        let mut id: Option<u64> = None;
        for _ in 0..8 {
            let Some(candidate) = Self::random_u64_best_effort() else {
                break;
            };
            if !self.entries.contains_key(&candidate) {
                id = Some(candidate);
                break;
            }
        }

        let mut id = id.unwrap_or_else(|| self.next_id.max(1));
        while self.entries.contains_key(&id) {
            id = id.wrapping_add(1).max(1);
        }
        self.next_id = id.wrapping_add(1).max(1);

        self.insert_entry(
            id,
            CursorStoreEntry {
                payload,
                expires_at: now + CURSOR_STORE_TTL,
            },
        );

        id
    }

    fn prune_expired(&mut self, now: Instant) {
        let mut expired: Vec<u64> = Vec::new();
        for (key, entry) in &self.entries {
            if entry.expires_at <= now {
                expired.push(*key);
            }
        }

        for key in expired {
            self.entries.remove(&key);
        }
        self.order.retain(|key| self.entries.contains_key(key));
    }

    fn acquire_persist_lock_best_effort(path: &Path) -> Option<std::fs::File> {
        let lock_path = path.with_extension("lock");
        let parent = lock_path.parent()?;
        std::fs::create_dir_all(parent).ok()?;

        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .ok()?;
        file.lock_exclusive().ok()?;
        Some(file)
    }

    fn load_persisted_best_effort(
        path: &Path,
        now_unix_ms: u64,
    ) -> (
        VecDeque<u64>,
        HashMap<u64, PersistedCursorStoreEntryData>,
        u64,
    ) {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(_) => return (VecDeque::new(), HashMap::new(), 0),
        };

        let persisted: PersistedCursorStore = match serde_json::from_slice(&bytes) {
            Ok(persisted) => persisted,
            Err(_) => return (VecDeque::new(), HashMap::new(), 0),
        };
        if persisted.v != 1 {
            return (VecDeque::new(), HashMap::new(), 0);
        }

        let mut order = VecDeque::new();
        let mut entries = HashMap::new();
        let mut max_id = 0u64;
        let mut seen_ids: HashSet<u64> = HashSet::new();

        for entry in persisted.entries {
            if entry.expires_at_unix_ms <= now_unix_ms {
                continue;
            }
            let Ok(payload) = STANDARD.decode(entry.payload_b64.as_bytes()) else {
                continue;
            };
            entries.insert(
                entry.id,
                PersistedCursorStoreEntryData {
                    payload,
                    expires_at_unix_ms: entry.expires_at_unix_ms,
                },
            );
            if seen_ids.insert(entry.id) {
                order.push_back(entry.id);
            }
            max_id = max_id.max(entry.id);
        }

        while order.len() > CURSOR_STORE_CAPACITY {
            if let Some(evicted) = order.pop_front() {
                entries.remove(&evicted);
            }
        }

        (order, entries, max_id)
    }

    fn persist_persisted_best_effort(
        path: &Path,
        order: &VecDeque<u64>,
        entries: &HashMap<u64, PersistedCursorStoreEntryData>,
    ) {
        let Some(parent) = path.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }

        let mut persisted_entries = Vec::new();
        for id in order {
            let Some(entry) = entries.get(id) else {
                continue;
            };
            persisted_entries.push(PersistedCursorStoreEntry {
                id: *id,
                expires_at_unix_ms: entry.expires_at_unix_ms,
                payload_b64: STANDARD.encode(&entry.payload),
            });
        }

        let persisted = PersistedCursorStore {
            v: 1,
            entries: persisted_entries,
        };
        let Ok(data) = serde_json::to_vec(&persisted) else {
            return;
        };

        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &data).is_err() {
            return;
        }
        let _ = std::fs::rename(&tmp, path);
    }

    fn load_best_effort(&mut self) {
        let Some(path) = self.persist_path.as_ref() else {
            return;
        };

        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(_) => return,
        };

        let persisted: PersistedCursorStore = match serde_json::from_slice(&bytes) {
            Ok(persisted) => persisted,
            Err(_) => return,
        };
        if persisted.v != 1 {
            return;
        }

        let now_unix_ms = unix_ms(SystemTime::now());
        let now_instant = Instant::now();

        let mut max_id = 0u64;
        let mut seen_ids: HashSet<u64> = HashSet::new();
        for entry in persisted.entries {
            if entry.expires_at_unix_ms <= now_unix_ms {
                continue;
            }
            let Ok(payload) = STANDARD.decode(entry.payload_b64.as_bytes()) else {
                continue;
            };
            let remaining_ms = entry.expires_at_unix_ms.saturating_sub(now_unix_ms);
            let expires_at = now_instant + Duration::from_millis(remaining_ms);
            self.entries.insert(
                entry.id,
                CursorStoreEntry {
                    payload,
                    expires_at,
                },
            );
            if seen_ids.insert(entry.id) {
                self.order.push_back(entry.id);
            }
            max_id = max_id.max(entry.id);
        }

        if !self.entries.is_empty() {
            self.next_id = max_id.wrapping_add(1).max(1);
        }

        while self.order.len() > CURSOR_STORE_CAPACITY {
            if let Some(evicted) = self.order.pop_front() {
                self.entries.remove(&evicted);
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedCursorStore {
    v: u32,
    entries: Vec<PersistedCursorStoreEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedCursorStoreEntry {
    id: u64,
    expires_at_unix_ms: u64,
    payload_b64: String,
}

fn cursor_store_persist_path() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("CONTEXT_MCP_CURSOR_STORE_PATH")
        .or_else(|_| std::env::var("CONTEXT_FINDER_MCP_CURSOR_STORE_PATH"))
    {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }

    let home = dirs::home_dir()?;
    let preferred = home
        .join(CONTEXT_DIR_NAME)
        .join("cache")
        .join("cursor_store_v1.json");
    if preferred.exists() {
        return Some(preferred);
    }
    let legacy = home
        .join(LEGACY_CONTEXT_DIR_NAME)
        .join("cache")
        .join("cursor_store_v1.json");
    if legacy.exists() {
        return Some(legacy);
    }
    Some(preferred)
}

#[derive(Default)]
struct SessionDefaults {
    /// Whether this connection completed an MCP `initialize` handshake in the current process.
    ///
    /// Some clients can reuse a shared-daemon transport across working directories and (buggily)
    /// issue tool calls without re-initializing. In daemon mode we fail-closed: do not persist or
    /// reuse session roots unless initialize has run.
    initialized: bool,
    root: Option<PathBuf>,
    root_display: Option<String>,
    focus_file: Option<String>,
    roots_pending: bool,
    /// Whether MCP `roots/list` returned multiple viable workspace roots and we refused to guess.
    ///
    /// In this state, callers must pass an explicit `path` (or an env override) to disambiguate.
    mcp_roots_ambiguous: bool,
    // Working-set: ephemeral, per-connection state (no disk). Used to avoid repeating the same
    // anchors/snippets across multiple calls in one agent session.
    seen_snippet_files: VecDeque<String>,
    seen_snippet_files_set: HashSet<String>,
}

impl SessionDefaults {
    fn clone_root(&self) -> Option<(PathBuf, String)> {
        Some((self.root.clone()?, self.root_display.clone()?))
    }

    fn reset_for_initialize(&mut self, roots_pending: bool) {
        self.initialized = true;
        self.root = None;
        self.root_display = None;
        self.focus_file = None;
        self.roots_pending = roots_pending;
        self.mcp_roots_ambiguous = false;
        self.clear_working_set();
    }

    fn set_root(&mut self, root: PathBuf, root_display: String, focus_file: Option<String>) {
        let root_changed = match self.root.as_ref() {
            Some(prev) => prev != &root,
            None => true,
        };
        self.root = Some(root);
        self.root_display = Some(root_display);
        self.focus_file = focus_file;
        self.mcp_roots_ambiguous = false;
        if root_changed {
            self.clear_working_set();
        }
    }

    fn clear_working_set(&mut self) {
        self.seen_snippet_files.clear();
        self.seen_snippet_files_set.clear();
    }

    fn note_seen_snippet_file(&mut self, file: &str) {
        const MAX_SEEN: usize = 160;

        let trimmed = file.trim();
        if trimmed.is_empty() {
            return;
        }
        if !self.seen_snippet_files_set.insert(trimmed.to_string()) {
            return;
        }
        self.seen_snippet_files.push_back(trimmed.to_string());
        while self.seen_snippet_files.len() > MAX_SEEN {
            if let Some(old) = self.seen_snippet_files.pop_front() {
                self.seen_snippet_files_set.remove(&old);
            }
        }
    }
}

fn trimmed_non_empty(input: Option<&str>) -> Option<&str> {
    input.map(str::trim).filter(|value| !value.is_empty())
}

fn resolve_root_from_absolute_hints(hints: &[String]) -> Option<PathBuf> {
    for hint in hints {
        let trimmed = hint.trim();
        if trimmed.is_empty() {
            continue;
        }
        let path = Path::new(trimmed);
        if !path.is_absolute() {
            continue;
        }
        if let Ok(root) = canonicalize_root_path(path) {
            return Some(root);
        }
    }
    None
}

fn collect_relative_hints(hints: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for hint in hints {
        let trimmed = hint.trim();
        if trimmed.is_empty() {
            continue;
        }
        let trimmed = trimmed.replace('\\', "/");
        if Path::new(&trimmed).is_absolute() {
            continue;
        }
        if is_glob_hint(&trimmed) {
            continue;
        }
        if !looks_like_path_hint(&trimmed) {
            continue;
        }
        out.push(trimmed);
        if out.len() >= 8 {
            break;
        }
    }
    out
}

fn hint_score_for_root(root: &Path, hints: &[String]) -> usize {
    let mut score = 0usize;
    for hint in hints {
        if root.join(hint).exists() {
            score = score.saturating_add(1);
        }
    }
    score
}

fn is_glob_hint(value: &str) -> bool {
    value.contains('*') || value.contains('?')
}

fn looks_like_path_hint(value: &str) -> bool {
    value.contains('/') || value.starts_with('.') || value.contains('.')
}

fn env_root_override() -> Option<(String, String)> {
    for key in [
        "CONTEXT_ROOT",
        "CONTEXT_PROJECT_ROOT",
        "CONTEXT_FINDER_ROOT",
        "CONTEXT_FINDER_PROJECT_ROOT",
    ] {
        if let Ok(value) = env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some((key.to_string(), trimmed.to_string()));
            }
        }
    }
    None
}

fn canonicalize_root(raw: &str) -> Result<PathBuf, String> {
    canonicalize_root_path(Path::new(raw))
}

fn canonicalize_root_path(path: &Path) -> Result<PathBuf, String> {
    let canonical = path.canonicalize().map_err(|err| err.to_string())?;

    // Agent-native UX: callers often pass a "current file" path as `path`.
    // Treat that as a hint within the project and prefer the enclosing git root (when present),
    // otherwise fall back to the file's parent directory.
    let (base, is_file) = match std::fs::metadata(&canonical) {
        Ok(meta) if meta.is_file() => (
            canonical
                .parent()
                .map(PathBuf::from)
                .ok_or_else(|| "Invalid path: file has no parent directory".to_string())?,
            true,
        ),
        _ => (canonical, false),
    };

    if is_file {
        if let Some(project_root) = find_project_root(&base) {
            return Ok(project_root);
        }
    }

    Ok(base)
}

fn rel_path_string(path: &Path) -> Option<String> {
    let raw = path.to_string_lossy().to_string();
    let normalized = raw.replace('\\', "/");
    let trimmed = normalized.trim().trim_start_matches("./");
    if trimmed.is_empty() || trimmed == "." {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(PathBuf::from)
}

fn find_project_root(start: &Path) -> Option<PathBuf> {
    if let Some(root) = find_git_root(start) {
        return Some(root);
    }

    const MARKERS: &[&str] = &[
        "AGENTS.md",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "CMakeLists.txt",
        "Makefile",
    ];

    start
        .ancestors()
        .find(|candidate| MARKERS.iter().any(|marker| candidate.join(marker).exists()))
        .map(PathBuf::from)
}

fn root_path_from_mcp_uri(uri: &str) -> Option<PathBuf> {
    let uri = uri.trim();
    if uri.is_empty() {
        return None;
    }

    // Only local file:// URIs are meaningful for a filesystem-indexing MCP server.
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_decode_utf8(rest)?;

    // file:///abs/path  -> "/abs/path"
    // file://localhost/abs/path -> "/abs/path"
    let decoded = decoded.strip_prefix("localhost").unwrap_or(&decoded);
    if !decoded.starts_with('/') {
        return None;
    }

    #[cfg(not(windows))]
    let path = decoded.to_string();

    // Windows file URIs are often "file:///C:/path" (leading slash before drive).
    #[cfg(windows)]
    let path = {
        let mut path = decoded.to_string();
        if path.len() >= 3
            && path.as_bytes()[0] == b'/'
            && path.as_bytes()[2] == b':'
            && path.as_bytes()[1].is_ascii_alphabetic()
        {
            path = path[1..].to_string();
        }
        path
    };

    Some(PathBuf::from(path))
}

fn percent_decode_utf8(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = *bytes.get(i + 1)?;
                let lo = *bytes.get(i + 2)?;
                let hi = (hi as char).to_digit(16)? as u8;
                let lo = (lo as char).to_digit(16)? as u8;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

struct EngineCache {
    capacity: usize,
    entries: HashMap<PathBuf, EngineHandle>,
    order: VecDeque<PathBuf>,
}

impl EngineCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get_or_insert(&mut self, root: &Path) -> EngineHandle {
        if let Some(handle) = self.entries.get(root).cloned() {
            self.touch(root);
            return handle;
        }

        let root = root.to_path_buf();
        let handle = Arc::new(Mutex::new(EngineSlot { engine: None }));
        self.entries.insert(root.clone(), handle.clone());
        self.touch(&root);

        while self.entries.len() > self.capacity {
            let Some(evict_root) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&evict_root);
        }

        handle
    }

    fn touch(&mut self, root: &Path) {
        if let Some(pos) = self.order.iter().position(|p| p.as_path() == root) {
            self.order.remove(pos);
        }
        self.order.push_back(root.to_path_buf());
    }

    fn recent_roots(&self) -> Vec<PathBuf> {
        self.order.iter().rev().cloned().collect()
    }
}

struct EngineSlot {
    engine: Option<ProjectEngine>,
}

struct EngineLock {
    slot: tokio::sync::OwnedMutexGuard<EngineSlot>,
}

impl EngineLock {
    fn engine_mut(&mut self) -> &mut ProjectEngine {
        self.slot.engine.as_mut().expect("engine must be available")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EngineSignature {
    corpus_mtime_ms: Option<u64>,
    index_mtimes_ms: Vec<(String, Option<u64>)>,
}

struct ProjectEngine {
    signature: EngineSignature,
    root: PathBuf,
    context_search: MultiModelContextSearch,
    chunk_lookup: HashMap<String, usize>,
    primary_model_id: String,
    semantic_index_cache_capacity: usize,
    semantic_index_lru: VecDeque<String>,
    canonical_index_mtime: SystemTime,
    graph_language: Option<GraphLanguage>,
}

#[derive(Debug, Default)]
struct EnsureSemanticModels {
    missing_models: Vec<String>,
    stale_models: Vec<String>,
}

impl ProjectEngine {
    fn loaded_model_ids(&self) -> Vec<String> {
        self.context_search.hybrid().loaded_model_ids()
    }

    async fn ensure_semantic_models_loaded(
        &mut self,
        project_watermark: &context_indexer::Watermark,
        model_ids: &[String],
    ) -> Result<EnsureSemanticModels> {
        let mut out = EnsureSemanticModels::default();

        let mut required: Vec<String> = model_ids
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect();
        required.sort();
        required.dedup();

        let required_set: HashSet<String> = required.iter().cloned().collect();

        for model_id in required {
            if model_id == self.primary_model_id {
                continue;
            }

            if self.context_search.hybrid().has_semantic_index(&model_id) {
                self.touch_semantic_lru(&model_id);
                continue;
            }

            let store_path = index_path_for_model(&self.root, &model_id);
            if !store_path.exists() {
                out.missing_models.push(model_id);
                continue;
            }

            let mut index_corrupt = false;
            let mut watermark = None;
            match read_index_watermark(&store_path).await {
                Ok(Some(PersistedIndexWatermark {
                    watermark: mark, ..
                })) => {
                    watermark = Some(mark);
                }
                Ok(None) => {}
                Err(_) => {
                    index_corrupt = true;
                }
            }

            let assessment = context_indexer::assess_staleness(
                project_watermark,
                true,
                index_corrupt,
                watermark.as_ref(),
            );
            if assessment.stale {
                out.stale_models.push(model_id);
                continue;
            }

            let index = VectorIndex::load(&store_path)
                .await
                .with_context(|| format!("Failed to load index {}", store_path.display()))?;
            self.context_search
                .hybrid_mut()
                .insert_semantic_index(model_id.clone(), index);
            self.touch_semantic_lru(&model_id);
        }

        self.evict_semantic_models(&required_set);

        Ok(out)
    }

    fn touch_semantic_lru(&mut self, model_id: &str) {
        if model_id == self.primary_model_id {
            return;
        }
        self.semantic_index_lru.retain(|m| m != model_id);
        self.semantic_index_lru.push_back(model_id.to_string());
    }

    fn evict_semantic_models(&mut self, required: &HashSet<String>) {
        let max_extra = self.semantic_index_cache_capacity.saturating_sub(1);
        while self.semantic_index_lru.len() > max_extra {
            let mut evicted = None;
            for candidate in &self.semantic_index_lru {
                if !required.contains(candidate) {
                    evicted = Some(candidate.clone());
                    break;
                }
            }

            let Some(model_id) = evicted else {
                break;
            };

            self.semantic_index_lru.retain(|m| m != &model_id);
            self.context_search
                .hybrid_mut()
                .remove_semantic_index(&model_id);
        }
    }

    async fn ensure_graph(&mut self, language: GraphLanguage) -> Result<()> {
        if self.graph_language == Some(language) && self.context_search.assembler().is_some() {
            return Ok(());
        }

        let cache = GraphCache::new(&self.root);
        match cache
            .load(
                self.canonical_index_mtime,
                language,
                self.context_search.hybrid().chunks(),
                &self.chunk_lookup,
            )
            .await
        {
            Ok(Some(assembler)) => {
                self.context_search.set_assembler(assembler);
                self.graph_language = Some(language);
                return Ok(());
            }
            Ok(None) => {}
            Err(err) => log::warn!("Graph cache load error: {err:#}"),
        }

        self.context_search.build_graph(language)?;
        self.graph_language = Some(language);

        if let Some(assembler) = self.context_search.assembler() {
            if let Err(err) = cache
                .save(self.canonical_index_mtime, language, assembler)
                .await
            {
                log::warn!("Graph cache save error: {err:#}");
            }
        }

        Ok(())
    }
}

#[derive(Clone)]
struct GraphCache {
    path: PathBuf,
}

impl GraphCache {
    fn new(project_root: &Path) -> Self {
        Self {
            path: context_dir_for_project_root(project_root).join("graph_cache.json"),
        }
    }

    async fn load(
        &self,
        store_mtime: SystemTime,
        language: GraphLanguage,
        chunks: &[context_code_chunker::CodeChunk],
        chunk_index: &HashMap<String, usize>,
    ) -> Result<Option<ContextAssembler>> {
        if !self.path.exists() {
            return Ok(None);
        }

        let data = match tokio::fs::read(&self.path).await {
            Ok(bytes) => bytes,
            Err(err) => {
                log::warn!("Failed to read graph cache {}: {err}", self.path.display());
                return Ok(None);
            }
        };

        let cached: CachedGraph = match serde_json::from_slice(&data) {
            Ok(cached) => cached,
            Err(err) => {
                log::warn!("Graph cache corrupted ({}): {err}", self.path.display());
                return Ok(None);
            }
        };

        if cached.language != language {
            return Ok(None);
        }

        if cached.index_mtime_ms != unix_ms(store_mtime) {
            return Ok(None);
        }

        let mut graph = CodeGraph::new();
        let mut node_indices = Vec::new();

        for node in cached.nodes {
            let Some(&idx) = chunk_index.get(&node.chunk_id) else {
                return Ok(None);
            };
            let Some(chunk) = chunks.get(idx) else {
                return Ok(None);
            };

            let graph_node = GraphNode {
                symbol: node.symbol,
                chunk_id: node.chunk_id,
                chunk: Some(chunk.clone()),
            };
            let idx = graph.add_node(graph_node);
            node_indices.push(idx);
        }

        for edge in cached.edges {
            let Some(&from_idx) = node_indices.get(edge.from) else {
                return Ok(None);
            };
            let Some(&to_idx) = node_indices.get(edge.to) else {
                return Ok(None);
            };
            graph.add_edge(
                from_idx,
                to_idx,
                GraphEdge {
                    relationship: edge.relationship,
                    weight: edge.weight,
                },
            );
        }

        Ok(Some(ContextAssembler::new(graph)))
    }

    async fn save(
        &self,
        store_mtime: SystemTime,
        language: GraphLanguage,
        assembler: &ContextAssembler,
    ) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let cached = CachedGraph::from_assembler(store_mtime, language, assembler);
        let data = serde_json::to_vec_pretty(&cached)?;
        tokio::fs::write(&self.path, data)
            .await
            .with_context(|| format!("Failed to write graph cache {}", self.path.display()))
    }
}

#[derive(Serialize, Deserialize)]
struct CachedGraph {
    index_mtime_ms: u64,
    language: GraphLanguage,
    nodes: Vec<CachedNode>,
    edges: Vec<CachedEdge>,
}

#[derive(Serialize, Deserialize)]
struct CachedNode {
    symbol: Symbol,
    chunk_id: String,
}

#[derive(Serialize, Deserialize)]
struct CachedEdge {
    from: usize,
    to: usize,
    relationship: RelationshipType,
    weight: f32,
}

impl CachedGraph {
    fn from_assembler(
        store_mtime: SystemTime,
        language: GraphLanguage,
        assembler: &ContextAssembler,
    ) -> Self {
        let graph = assembler.graph();
        let mut node_map = HashMap::new();
        let mut nodes = Vec::new();

        for (idx, node) in graph.graph.node_indices().enumerate() {
            if let Some(data) = graph.graph.node_weight(node) {
                node_map.insert(node, idx);
                nodes.push(CachedNode {
                    symbol: data.symbol.clone(),
                    chunk_id: data.chunk_id.clone(),
                });
            }
        }

        let mut edges = Vec::new();
        for edge_id in graph.graph.edge_indices() {
            let Some((source, target)) = graph.graph.edge_endpoints(edge_id) else {
                continue;
            };
            let Some(weight) = graph.graph.edge_weight(edge_id) else {
                continue;
            };
            let (Some(&from), Some(&to)) = (node_map.get(&source), node_map.get(&target)) else {
                continue;
            };
            edges.push(CachedEdge {
                from,
                to,
                relationship: weight.relationship,
                weight: weight.weight,
            });
        }

        Self {
            index_mtime_ms: unix_ms(store_mtime),
            language,
            nodes,
            edges,
        }
    }
}

async fn compute_engine_signature(root: &Path, models: &[String]) -> Result<EngineSignature> {
    let corpus_path = corpus_path_for_project_root(root);
    let corpus_mtime_ms = tokio::fs::metadata(&corpus_path)
        .await
        .and_then(|m| m.modified())
        .ok()
        .map(unix_ms);

    let mut models: Vec<String> = models
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();
    models.sort();
    models.dedup();
    if models.is_empty() {
        models.push(current_model_id().unwrap_or_else(|_| "bge-small".to_string()));
    }

    let mut index_mtimes_ms = Vec::with_capacity(models.len());
    for model_id in models {
        let index_path = index_path_for_model(root, &model_id);
        let mtime_ms = tokio::fs::metadata(&index_path)
            .await
            .and_then(|m| m.modified())
            .ok()
            .map(unix_ms);
        index_mtimes_ms.push((model_id, mtime_ms));
    }

    Ok(EngineSignature {
        corpus_mtime_ms,
        index_mtimes_ms,
    })
}

async fn build_project_engine(
    root: &Path,
    profile: &SearchProfile,
    signature: EngineSignature,
) -> Result<ProjectEngine> {
    let sources = load_semantic_indexes(root).await?;
    let canonical_model_id = sources
        .first()
        .map(|(id, _)| id.clone())
        .ok_or_else(|| anyhow::anyhow!("No semantic indices available"))?;
    let canonical_index_path = index_path_for_model(root, &canonical_model_id);
    let primary_model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    let canonical_index_mtime = tokio::fs::metadata(&canonical_index_path)
        .await
        .with_context(|| format!("Failed to stat {}", canonical_index_path.display()))?
        .modified()
        .with_context(|| format!("Failed to read mtime {}", canonical_index_path.display()))?;

    let corpus = ContextFinderService::load_chunk_corpus(root).await?;
    let hybrid = match corpus {
        Some(corpus) => {
            MultiModelHybridSearch::from_env_with_corpus(sources, profile.clone(), corpus)
        }
        None => MultiModelHybridSearch::from_env(sources, profile.clone()),
    }?;

    let context_search = MultiModelContextSearch::new(hybrid)?;
    let chunk_lookup = build_chunk_lookup(context_search.hybrid().chunks());

    Ok(ProjectEngine {
        signature,
        root: root.to_path_buf(),
        context_search,
        chunk_lookup,
        primary_model_id,
        semantic_index_cache_capacity: engine_semantic_index_cache_capacity_from_env(),
        semantic_index_lru: VecDeque::new(),
        canonical_index_mtime,
        graph_language: None,
    })
}

// ============================================================================
// Tool Implementations
// ============================================================================

mod read_pack;
mod router;

fn strip_structured_content(mut result: CallToolResult) -> CallToolResult {
    result.structured_content = None;
    result
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
            router::capabilities::capabilities(self, request).await?,
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
            router::help::help(self, request).await?,
        ))
    }

    /// Project structure overview (tree-like).
    #[tool(description = "Project structure overview with directories, files, and top symbols.")]
    pub async fn tree(
        &self,
        Parameters(request): Parameters<MapRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(strip_structured_content(
            router::map::map(self, request).await?,
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
            router::repo_onboarding_pack::repo_onboarding_pack(self, request).await?,
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
            router::meaning_pack::meaning_pack(self, request).await?,
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
            router::meaning_focus::meaning_focus(self, request).await?,
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
            router::worktree_pack::worktree_pack(self, request).await?,
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
            router::atlas_pack::atlas_pack(self, request).await?,
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
            router::notebook_pack::notebook_pack(self, request).await?,
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
            router::notebook_edit::notebook_edit(self, request).await?,
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
            router::notebook_apply_suggest::notebook_apply_suggest(self, request).await?,
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
            router::notebook_suggest::notebook_suggest(self, request).await?,
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
            router::runbook_pack::runbook_pack(self, request).await?,
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
            router::text_search::text_search(self, request).await?,
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
            router::file_slice::file_slice(self, &request).await?,
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
            router::evidence_fetch::evidence_fetch(self, request).await?,
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
            router::read_pack::read_pack(self, request).await?,
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
            router::ls::ls(self, request).await?,
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
            router::list_files::list_files(self, request).await?,
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
            router::grep_context::grep_context(self, request).await?,
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
            router::grep_context::grep_context(self, request).await?,
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
            router::batch::batch(self, request).await?,
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
            router::doctor::doctor(self, request).await?,
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
            router::search::search(self, request).await?,
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
            router::context::context(self, request).await?,
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
            router::context_pack::context_pack(self, request).await?,
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
            router::impact::impact(self, request).await?,
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
            router::trace::trace(self, request).await?,
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
            router::explain::explain(self, request).await?,
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
            router::overview::overview(self, request).await?,
        ))
    }
}

fn finalize_read_pack_budget(result: &mut ReadPackResult) -> anyhow::Result<()> {
    finalize_used_chars(result, |inner, used| inner.budget.used_chars = used).map(|_| ())
}
// ============================================================================
// Helper functions
// ============================================================================

impl ContextFinderService {
    fn find_text_usages(
        chunks: &[context_code_chunker::CodeChunk],
        symbol: &str,
        exclude_chunk_id: Option<&str>,
        max_results: usize,
    ) -> Vec<UsageInfo> {
        if symbol.is_empty() || max_results == 0 {
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut seen: HashSet<(String, usize)> = HashSet::new();

        for chunk in chunks {
            if out.len() >= max_results {
                break;
            }

            if path_has_extension_ignore_ascii_case(&chunk.file_path, "md") {
                continue;
            }

            let chunk_id = format!(
                "{}:{}:{}",
                chunk.file_path, chunk.start_line, chunk.end_line
            );
            if let Some(exclude) = exclude_chunk_id {
                if chunk_id == exclude {
                    continue;
                }
            }

            let Some(hit_byte) = Self::find_word_boundary(&chunk.content, symbol) else {
                continue;
            };

            let line_offset = chunk.content[..hit_byte]
                .bytes()
                .filter(|b| *b == b'\n')
                .count();
            let line = chunk.start_line + line_offset;
            if !seen.insert((chunk.file_path.clone(), line)) {
                continue;
            }

            out.push(UsageInfo {
                file: chunk.file_path.clone(),
                line,
                symbol: chunk
                    .metadata
                    .symbol_name
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                relationship: "TextMatch".to_string(),
            });
        }

        out
    }

    fn find_word_boundary(haystack: &str, needle: &str) -> Option<usize> {
        if needle.is_empty() {
            return None;
        }

        let needle_is_ident = needle.bytes().all(Self::is_ident_byte);
        if !needle_is_ident {
            return haystack.find(needle);
        }

        let bytes = haystack.as_bytes();
        for (idx, _) in haystack.match_indices(needle) {
            let left_ok = idx == 0 || !Self::is_ident_byte(bytes[idx - 1]);
            let right_idx = idx + needle.len();
            let right_ok = right_idx >= bytes.len() || !Self::is_ident_byte(bytes[right_idx]);
            if left_ok && right_ok {
                return Some(idx);
            }
        }
        None
    }

    const fn is_ident_byte(b: u8) -> bool {
        matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
    }

    fn match_in_line(
        line: &str,
        pattern: &str,
        case_sensitive: bool,
        whole_word: bool,
    ) -> Option<usize> {
        if case_sensitive {
            if whole_word {
                Self::find_word_boundary(line, pattern)
            } else {
                line.find(pattern)
            }
        } else {
            let line_lower = line.to_ascii_lowercase();
            let pat_lower = pattern.to_ascii_lowercase();
            if whole_word {
                Self::find_word_boundary(&line_lower, &pat_lower)
            } else {
                line_lower.find(&pat_lower)
            }
        }
    }

    pub(super) fn matches_file_pattern(path: &str, pattern: Option<&str>) -> bool {
        let Some(pattern) = pattern else {
            return true;
        };
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return true;
        }

        if !pattern.contains('*') && !pattern.contains('?') {
            return path.contains(pattern);
        }

        Self::glob_match(pattern, path)
    }

    // Minimal glob matcher supporting '*' and '?'.
    fn glob_match(pattern: &str, text: &str) -> bool {
        let p = pattern.as_bytes();
        let t = text.as_bytes();
        let mut p_idx = 0usize;
        let mut t_idx = 0usize;
        let mut star_idx: Option<usize> = None;
        let mut match_idx = 0usize;

        while t_idx < t.len() {
            if p_idx < p.len() && (p[p_idx] == b'?' || p[p_idx] == t[t_idx]) {
                p_idx += 1;
                t_idx += 1;
                continue;
            }

            if p_idx < p.len() && p[p_idx] == b'*' {
                star_idx = Some(p_idx);
                match_idx = t_idx;
                p_idx += 1;
                continue;
            }

            if let Some(star) = star_idx {
                p_idx = star + 1;
                match_idx += 1;
                t_idx = match_idx;
                continue;
            }

            return false;
        }

        while p_idx < p.len() && p[p_idx] == b'*' {
            p_idx += 1;
        }
        p_idx == p.len()
    }

    fn parse_language(lang: Option<&str>) -> GraphLanguage {
        match lang {
            Some("python") => GraphLanguage::Python,
            Some("javascript") => GraphLanguage::JavaScript,
            Some("typescript") => GraphLanguage::TypeScript,
            _ => GraphLanguage::Rust,
        }
    }

    /// Auto-detect primary language from file extensions in chunks
    fn detect_language(chunks: &[context_code_chunker::CodeChunk]) -> GraphLanguage {
        let mut rust_count = 0;
        let mut python_count = 0;
        let mut js_count = 0;
        let mut ts_count = 0;

        for chunk in chunks {
            if path_has_extension_ignore_ascii_case(&chunk.file_path, "rs") {
                rust_count += 1;
            } else if path_has_extension_ignore_ascii_case(&chunk.file_path, "py") {
                python_count += 1;
            } else if path_has_extension_ignore_ascii_case(&chunk.file_path, "ts")
                || path_has_extension_ignore_ascii_case(&chunk.file_path, "tsx")
            {
                ts_count += 1;
            } else if path_has_extension_ignore_ascii_case(&chunk.file_path, "js")
                || path_has_extension_ignore_ascii_case(&chunk.file_path, "jsx")
            {
                js_count += 1;
            }
        }

        let max = rust_count.max(python_count).max(js_count).max(ts_count);
        if max == 0 {
            return GraphLanguage::Rust; // default
        }
        if max == rust_count {
            GraphLanguage::Rust
        } else if max == python_count {
            GraphLanguage::Python
        } else if max == ts_count {
            GraphLanguage::TypeScript
        } else {
            GraphLanguage::JavaScript
        }
    }

    fn guess_layer_role(name: &str) -> String {
        match name.to_lowercase().as_str() {
            "cli" | "cmd" | "bin" => "Command-line interface".to_string(),
            "api" | "server" | "web" => "API/Server layer".to_string(),
            "core" | "lib" | "src" => "Core library".to_string(),
            "test" | "tests" => "Test suite".to_string(),
            "crates" => "Workspace crates".to_string(),
            "docs" | "doc" => "Documentation".to_string(),
            _ => "Module".to_string(),
        }
    }

    fn generate_impact_mermaid(
        symbol: &str,
        direct: &[UsageInfo],
        transitive: &[UsageInfo],
    ) -> String {
        let mut lines = vec!["graph LR".to_string()];

        // Add direct edges
        for usage in direct.iter().take(10) {
            lines.push(format!(
                "    {}-->|{}|{}",
                Self::mermaid_safe(&usage.symbol),
                usage.relationship,
                Self::mermaid_safe(symbol)
            ));
        }

        // Add transitive edges (simplified)
        for usage in transitive.iter().take(5) {
            lines.push(format!(
                "    {}-.->|transitive|{}",
                Self::mermaid_safe(&usage.symbol),
                Self::mermaid_safe(symbol)
            ));
        }

        lines.join("\n")
    }

    fn generate_trace_mermaid(steps: &[TraceStep]) -> String {
        if steps.is_empty() {
            return "sequenceDiagram\n    Note over A: No path found".to_string();
        }

        let mut lines = vec!["sequenceDiagram".to_string()];

        for window in steps.windows(2) {
            let from = &window[0];
            let to = &window[1];
            let rel = to.relationship.as_deref().unwrap_or("calls");
            lines.push(format!(
                "    {}->>{}+: {}",
                Self::mermaid_safe(&from.symbol),
                Self::mermaid_safe(&to.symbol),
                rel
            ));
        }

        lines.join("\n")
    }

    fn mermaid_safe(s: &str) -> String {
        s.replace("::", "_").replace(['<', '>', ' '], "_")
    }
}

const fn graph_language_key(language: GraphLanguage) -> &'static str {
    match language {
        GraphLanguage::Rust => "rust",
        GraphLanguage::Python => "python",
        GraphLanguage::JavaScript => "javascript",
        GraphLanguage::TypeScript => "typescript",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RelatedMode {
    Explore,
    Focus,
}

impl RelatedMode {
    #[must_use]
    const fn as_str(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::Focus => "focus",
        }
    }
}

fn tokenize_focus_query(query: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        // English.
        "a",
        "an",
        "and",
        "are",
        "as",
        "at",
        "be",
        "by",
        "for",
        "from",
        "how",
        "in",
        "is",
        "it",
        "of",
        "on",
        "or",
        "that",
        "the",
        "this",
        "to",
        "what",
        "when",
        "where",
        "why",
        "with",
        // Common repo/path noise.
        "bin",
        "crates",
        "doc",
        "docs",
        "lib",
        "src",
        "test",
        "tests",
        // Common extensions.
        "c",
        "cpp",
        "go",
        "h",
        "hpp",
        "java",
        "js",
        "json",
        "md",
        "mdx",
        "py",
        "rs",
        "toml",
        "ts",
        "yaml",
        "yml",
        // Russian.
        "в",
        "для",
        "и",
        "или",
        "как",
        "на",
        "по",
        "почему",
        "что",
        "где",
        "зачем",
    ];

    let q = query.to_ascii_lowercase();
    let mut tokens = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for raw in q.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-') {
        let token = raw.trim();
        if token.is_empty() || token.len() < 2 {
            continue;
        }
        if STOPWORDS.contains(&token) {
            continue;
        }
        if seen.insert(token.to_string()) {
            tokens.push(token.to_string());
        }
        if tokens.len() >= 12 {
            break;
        }
    }
    tokens
}

fn related_query_hit(rc: &context_search::RelatedContext, query_tokens: &[String]) -> bool {
    if query_tokens.is_empty() {
        return false;
    }

    let file = rc.chunk.file_path.to_ascii_lowercase();
    if query_tokens.iter().any(|t| file.contains(t)) {
        return true;
    }

    if let Some(symbol) = rc.chunk.metadata.symbol_name.as_deref() {
        let symbol = symbol.to_ascii_lowercase();
        if query_tokens.iter().any(|t| symbol.contains(t)) {
            return true;
        }
    }

    let content = rc.chunk.content.to_ascii_lowercase();
    query_tokens.iter().any(|t| content.contains(t))
}

fn prepare_related_contexts(
    mut related: Vec<context_search::RelatedContext>,
    related_mode: RelatedMode,
    query_tokens: &[String],
) -> Vec<context_search::RelatedContext> {
    let explore_sort = |a: &context_search::RelatedContext, b: &context_search::RelatedContext| {
        b.relevance_score
            .total_cmp(&a.relevance_score)
            .then_with(|| a.distance.cmp(&b.distance))
            .then_with(|| a.chunk.file_path.cmp(&b.chunk.file_path))
            .then_with(|| a.chunk.start_line.cmp(&b.chunk.start_line))
    };

    match related_mode {
        RelatedMode::Explore => {
            related.sort_by(explore_sort);
            related
        }
        RelatedMode::Focus => {
            const MAX_DISTANCE_FOCUS: usize = 2;
            const FALLBACK_NON_HITS: usize = 2;

            if query_tokens.is_empty() {
                related.sort_by(explore_sort);
                return related;
            }

            related.retain(|rc| rc.distance <= MAX_DISTANCE_FOCUS);

            let mut hits: Vec<context_search::RelatedContext> = Vec::new();
            let mut misses: Vec<context_search::RelatedContext> = Vec::new();
            for rc in related {
                if related_query_hit(&rc, query_tokens) {
                    hits.push(rc);
                } else {
                    misses.push(rc);
                }
            }

            if !hits.is_empty() {
                misses.retain(|rc| rc.distance <= 1);
            }
            misses.sort_by(explore_sort);
            misses.truncate(FALLBACK_NON_HITS);
            let fallback = misses;

            let mut combined: Vec<(bool, context_search::RelatedContext)> =
                hits.into_iter().map(|rc| (true, rc)).collect();
            combined.extend(fallback.into_iter().map(|rc| (false, rc)));

            combined.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| b.1.relevance_score.total_cmp(&a.1.relevance_score))
                    .then_with(|| a.1.distance.cmp(&b.1.distance))
                    .then_with(|| a.1.chunk.file_path.cmp(&b.1.chunk.file_path))
                    .then_with(|| a.1.chunk.start_line.cmp(&b.1.chunk.start_line))
            });

            combined.into_iter().map(|(_, rc)| rc).collect()
        }
    }
}

fn prepare_context_pack_enriched(
    mut enriched: Vec<context_search::EnrichedResult>,
    limit: usize,
    prefer_code: bool,
    include_docs: bool,
) -> Vec<context_search::EnrichedResult> {
    if !include_docs {
        enriched.retain(|er| classify_path_kind(&er.primary.chunk.file_path) != DocumentKind::Docs);
    }

    enriched.sort_by(|a, b| {
        let a_kind = classify_path_kind(&a.primary.chunk.file_path);
        let b_kind = classify_path_kind(&b.primary.chunk.file_path);
        document_kind_rank(a_kind, prefer_code)
            .cmp(&document_kind_rank(b_kind, prefer_code))
            .then_with(|| b.primary.score.total_cmp(&a.primary.score))
            .then_with(|| a.primary.chunk.file_path.cmp(&b.primary.chunk.file_path))
            .then_with(|| a.primary.chunk.start_line.cmp(&b.primary.chunk.start_line))
    });

    if !include_docs {
        for er in &mut enriched {
            er.related
                .retain(|rc| classify_path_kind(&rc.chunk.file_path) != DocumentKind::Docs);
        }
    }

    enriched.truncate(limit);
    enriched
}

const fn document_kind_rank(kind: DocumentKind, prefer_code: bool) -> u8 {
    if prefer_code {
        match kind {
            DocumentKind::Code => 0,
            DocumentKind::Test => 1,
            DocumentKind::Config => 2,
            DocumentKind::Other => 3,
            DocumentKind::Docs => 4,
        }
    } else {
        match kind {
            DocumentKind::Docs => 0,
            DocumentKind::Code => 1,
            DocumentKind::Test => 2,
            DocumentKind::Config => 3,
            DocumentKind::Other => 4,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn pack_enriched_results(
    profile: &SearchProfile,
    enriched: Vec<context_search::EnrichedResult>,
    max_chars: usize,
    max_related_per_primary: usize,
    include_paths: &[String],
    exclude_paths: &[String],
    file_pattern: Option<&str>,
    related_mode: RelatedMode,
    query_tokens: &[String],
) -> (Vec<ContextPackItem>, ContextPackBudget) {
    let mut used_chars = 0usize;
    let mut truncated = false;
    let mut dropped_items = 0usize;

    let mut items: Vec<ContextPackItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let mut related_queues: Vec<VecDeque<context_search::RelatedContext>> = Vec::new();
    let mut selected_related: Vec<usize> = Vec::new();
    let mut per_relationship: Vec<HashMap<String, usize>> = Vec::new();

    fn truncate_to_byte_len_at_char_boundary(text: &mut String, max_bytes: usize) {
        if text.len() <= max_bytes {
            return;
        }

        let mut cut = max_bytes.min(text.len());
        while cut > 0 && !text.is_char_boundary(cut) {
            cut = cut.saturating_sub(1);
        }
        text.truncate(cut);
    }

    for er in enriched {
        let primary = er.primary;
        if !seen.insert(primary.id.clone()) {
            continue;
        }
        if !context_protocol::path_filters::path_allowed(
            &primary.chunk.file_path,
            include_paths,
            exclude_paths,
            file_pattern,
        ) {
            continue;
        }

        let primary_item = build_primary_item(primary);
        let cost = estimate_item_chars(&primary_item);
        if used_chars.saturating_add(cost) > max_chars {
            truncated = true;

            // Always return at least one anchor item. Even if the first primary chunk is huge,
            // shrink it aggressively instead of returning `0 items` (which breaks agent flows).
            if items.is_empty() {
                const ANCHOR_ITEM_OVERHEAD_CHARS: usize = 512;
                let mut anchor = primary_item;
                anchor.imports.clear();
                truncate_to_byte_len_at_char_boundary(
                    &mut anchor.content,
                    max_chars.saturating_sub(ANCHOR_ITEM_OVERHEAD_CHARS).max(1),
                );
                used_chars = estimate_item_chars(&anchor);
                items.push(anchor);
            } else {
                dropped_items += 1;
            }
            break;
        }
        used_chars += cost;
        items.push(primary_item);

        let mut related = er.related;
        related.retain(|rc| !profile.is_rejected(&rc.chunk.file_path));
        related.retain(|rc| {
            context_protocol::path_filters::path_allowed(
                &rc.chunk.file_path,
                include_paths,
                exclude_paths,
                file_pattern,
            )
        });
        let related = prepare_related_contexts(related, related_mode, query_tokens);
        related_queues.push(VecDeque::from(related));
        selected_related.push(0);
        per_relationship.push(HashMap::new());
    }

    'outer_related: while !truncated {
        let mut added_any = false;
        for idx in 0..related_queues.len() {
            if selected_related[idx] >= max_related_per_primary {
                continue;
            }

            let queue = &mut related_queues[idx];
            while let Some(rc) = queue.pop_front() {
                let kind = rc
                    .relationship_path
                    .first()
                    .cloned()
                    .unwrap_or_else(String::new);
                let cap = relationship_cap(&kind);
                let used = per_relationship[idx]
                    .get(kind.as_str())
                    .copied()
                    .unwrap_or(0);
                if used >= cap {
                    continue;
                }

                let id = chunk_id(&rc.chunk.file_path, rc.chunk.start_line, rc.chunk.end_line);
                if !seen.insert(id.clone()) {
                    continue;
                }

                let item = build_related_item(id, rc);

                let cost = estimate_item_chars(&item);
                if used_chars.saturating_add(cost) > max_chars {
                    truncated = true;
                    dropped_items += 1;
                    break 'outer_related;
                }

                used_chars += cost;
                items.push(item);
                *per_relationship[idx].entry(kind).or_insert(0) += 1;
                selected_related[idx] += 1;
                added_any = true;
                break;
            }
        }

        if !added_any {
            break;
        }
    }

    (
        items,
        ContextPackBudget {
            max_chars,
            used_chars,
            truncated,
            dropped_items,
            truncation: truncated.then_some(BudgetTruncation::MaxChars),
        },
    )
}

fn relationship_cap(kind: &str) -> usize {
    match kind {
        "Calls" | "Uses" => 6,
        "Contains" => 4,
        "Extends" => 3,
        _ => 2,
    }
}

fn chunk_id(file: &str, start_line: usize, end_line: usize) -> String {
    format!("{file}:{start_line}:{end_line}")
}

fn sanitize_score(score: f32) -> f32 {
    if !score.is_finite() {
        return 0.0;
    }
    score.clamp(0.0, 1.0)
}

fn build_primary_item(primary: context_search::SearchResult) -> ContextPackItem {
    let context_search::SearchResult { chunk, score, id } = primary;
    ContextPackItem {
        id,
        role: "primary".to_string(),
        file: chunk.file_path,
        start_line: chunk.start_line,
        end_line: chunk.end_line,
        symbol: chunk.metadata.symbol_name,
        chunk_type: chunk.metadata.chunk_type.map(|ct| ct.as_str().to_string()),
        score: sanitize_score(score),
        imports: chunk.metadata.context_imports,
        content: chunk.content,
        relationship: None,
        distance: None,
    }
}

fn build_related_item(id: String, rc: context_search::RelatedContext) -> ContextPackItem {
    ContextPackItem {
        id,
        role: "related".to_string(),
        file: rc.chunk.file_path,
        start_line: rc.chunk.start_line,
        end_line: rc.chunk.end_line,
        symbol: rc.chunk.metadata.symbol_name,
        chunk_type: rc
            .chunk
            .metadata
            .chunk_type
            .map(|ct| ct.as_str().to_string()),
        score: sanitize_score(rc.relevance_score),
        imports: rc.chunk.metadata.context_imports,
        content: rc.chunk.content,
        relationship: Some(rc.relationship_path),
        distance: Some(rc.distance),
    }
}

fn estimate_item_chars(item: &ContextPackItem) -> usize {
    let imports: usize = item.imports.iter().map(|s| s.len() + 1).sum();
    item.content.len() + imports + 128
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_code_chunker::ChunkMetadata;
    use context_search::{EnrichedResult, RelatedContext};
    use context_vector_store::SearchResult;
    use tempfile::tempdir;
    use tokio::time::Duration;

    #[test]
    fn word_boundary_match_hits_only_whole_identifier() {
        assert!(ContextFinderService::find_word_boundary("fn new() {}", "new").is_some());
        assert!(ContextFinderService::find_word_boundary("renew", "new").is_none());
        assert!(ContextFinderService::find_word_boundary("news", "new").is_none());
        assert!(ContextFinderService::find_word_boundary("new_", "new").is_none());
        assert!(ContextFinderService::find_word_boundary(" new ", "new").is_some());
    }

    #[test]
    fn text_usages_compute_line_and_respect_exclusion() {
        let chunk = context_code_chunker::CodeChunk::new(
            "a.rs".to_string(),
            10,
            20,
            "fn caller() {\n  touch_daemon_best_effort();\n}\n".to_string(),
            ChunkMetadata::default()
                .symbol_name("caller")
                .chunk_type(context_code_chunker::ChunkType::Function),
        );

        let usages = ContextFinderService::find_text_usages(
            std::slice::from_ref(&chunk),
            "touch_daemon_best_effort",
            None,
            10,
        );
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].file, "a.rs");
        assert_eq!(usages[0].line, 11);
        assert_eq!(usages[0].symbol, "caller");
        assert_eq!(usages[0].relationship, "TextMatch");

        let exclude = format!(
            "{}:{}:{}",
            chunk.file_path, chunk.start_line, chunk.end_line
        );
        let excluded = ContextFinderService::find_text_usages(
            &[chunk],
            "touch_daemon_best_effort",
            Some(&exclude),
            10,
        );
        assert!(excluded.is_empty());
    }

    #[tokio::test]
    async fn resolve_root_waits_for_initialize_roots_list() {
        let dir = tempdir().expect("temp dir");
        let canonical_root = dir.path().canonicalize().expect("canonical root");
        let canonical_root_clone = canonical_root.clone();
        let canonical_display = canonical_root.to_string_lossy().to_string();

        let service = ContextFinderService::new_daemon();
        {
            let mut session = service.session.lock().await;
            session.reset_for_initialize(true);
        }

        let session_arc = service.session.clone();
        let notify = service.roots_notify.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut session = session_arc.lock().await;
            session.root = Some(canonical_root_clone);
            session.root_display = Some(canonical_display);
            session.roots_pending = false;
            drop(session);
            notify.notify_waiters();
        });

        let (root, _) = service
            .resolve_root_no_daemon_touch(None)
            .await
            .expect("root must resolve after roots/list");
        assert_eq!(root, canonical_root);
    }

    #[tokio::test]
    async fn daemon_does_not_reuse_or_persist_root_without_initialize() {
        let dir = tempdir().expect("temp dir");
        let canonical_root = dir.path().canonicalize().expect("canonical root");
        let root_str = canonical_root.to_string_lossy().to_string();

        let service = ContextFinderService::new_daemon();

        let (resolved, _) = service
            .resolve_root_no_daemon_touch(Some(&root_str))
            .await
            .expect("root must resolve with explicit path");
        assert_eq!(resolved, canonical_root);

        let err = service
            .resolve_root_no_daemon_touch(None)
            .await
            .expect_err("expected missing root without initialize");
        assert!(
            err.contains("Missing project root"),
            "expected error to mention missing project root"
        );

        {
            let mut session = service.session.lock().await;
            session.reset_for_initialize(false);
        }

        let (resolved, _) = service
            .resolve_root_no_daemon_touch(Some(&root_str))
            .await
            .expect("root must resolve after initialize");
        assert_eq!(resolved, canonical_root);

        let (resolved, _) = service
            .resolve_root_no_daemon_touch(None)
            .await
            .expect("expected sticky root after initialize");
        assert_eq!(resolved, canonical_root);
    }

    #[tokio::test]
    async fn daemon_refuses_cross_project_root_inference_from_relative_hints() {
        let root_a = tempdir().expect("temp root_a");
        std::fs::create_dir_all(root_a.path().join("src")).expect("mkdir src");
        std::fs::write(root_a.path().join("src").join("main.rs"), "fn main() {}\n")
            .expect("write src/main.rs");
        let root_a = root_a.path().canonicalize().expect("canonical root_a");

        // Simulate another connection having recently touched a different root.
        let svc_a = ContextFinderService::new_daemon().clone_for_connection();
        svc_a.state.engine_handle(&root_a).await;

        // New connection with no explicit path should fail-closed, rather than guessing a root
        // from shared (cross-session) recent_roots based on relative file hints.
        let svc_b = svc_a.clone_for_connection();
        let err = svc_b
            .resolve_root_with_hints_no_daemon_touch(None, &["src/main.rs".to_string()])
            .await
            .expect_err("expected daemon to refuse root inference from relative hints");
        assert!(
            err.contains("Missing project root"),
            "expected missing-root error, got: {err}"
        );
    }

    #[test]
    fn context_pack_prefers_more_primary_items_under_tight_budgets() {
        let make_chunk =
            |file: &str, start: usize, end: usize, symbol: &str, content_len: usize| {
                let content = "x".repeat(content_len);
                context_code_chunker::CodeChunk::new(
                    file.to_string(),
                    start,
                    end,
                    content,
                    ChunkMetadata::default()
                        .symbol_name(symbol)
                        .chunk_type(context_code_chunker::ChunkType::Function),
                )
            };

        let primary = |file: &str, id: &str, symbol: &str| SearchResult {
            chunk: make_chunk(file, 1, 10, symbol, 40),
            score: 1.0,
            id: id.to_string(),
        };

        let related = |file: &str, symbol: &str| RelatedContext {
            chunk: make_chunk(file, 1, 200, symbol, 1_000),
            relationship_path: vec!["Calls".to_string()],
            distance: 1,
            relevance_score: 0.5,
        };

        let enriched = vec![
            EnrichedResult {
                primary: primary("src/a.rs", "src/a.rs:1:10", "a"),
                related: vec![related("src/a_related.rs", "a_related")],
                total_lines: 10,
                strategy: context_graph::AssemblyStrategy::Direct,
            },
            EnrichedResult {
                primary: primary("src/b.rs", "src/b.rs:1:10", "b"),
                related: vec![related("src/b_related.rs", "b_related")],
                total_lines: 10,
                strategy: context_graph::AssemblyStrategy::Direct,
            },
            EnrichedResult {
                primary: primary("src/c.rs", "src/c.rs:1:10", "c"),
                related: vec![related("src/c_related.rs", "c_related")],
                total_lines: 10,
                strategy: context_graph::AssemblyStrategy::Direct,
            },
        ];

        let profile = SearchProfile::general();
        let max_chars = 900;
        let (items, budget) = pack_enriched_results(
            &profile,
            enriched,
            max_chars,
            3,
            &[],
            &[],
            None,
            RelatedMode::Explore,
            &[],
        );

        let primary_count = items.iter().filter(|i| i.role == "primary").count();
        assert_eq!(primary_count, 3, "expected all primaries to fit");
        assert!(
            items.iter().take(3).all(|i| i.role == "primary"),
            "expected primaries to be emitted before related items"
        );
        assert!(
            budget.used_chars <= max_chars,
            "expected budget.used_chars <= max_chars"
        );
        assert!(
            budget.truncated,
            "expected related items to trigger truncation under tight max_chars"
        );
    }

    #[test]
    fn context_pack_never_returns_zero_items_when_first_chunk_is_huge() {
        let chunk = context_code_chunker::CodeChunk::new(
            "src/big.rs".to_string(),
            1,
            999,
            "x".repeat(10_000),
            ChunkMetadata::default()
                .symbol_name("huge")
                .chunk_type(context_code_chunker::ChunkType::Function),
        );

        let enriched = vec![EnrichedResult {
            primary: SearchResult {
                chunk,
                score: 1.0,
                id: "src/big.rs:1:999".to_string(),
            },
            related: vec![],
            total_lines: 999,
            strategy: context_graph::AssemblyStrategy::Direct,
        }];

        let profile = SearchProfile::general();
        let max_chars = 1_000;
        let (items, budget) = pack_enriched_results(
            &profile,
            enriched,
            max_chars,
            0,
            &[],
            &[],
            None,
            RelatedMode::Explore,
            &[],
        );

        assert_eq!(items.len(), 1, "expected an anchor item");
        assert_eq!(items[0].role, "primary");
        assert!(
            !items[0].content.is_empty(),
            "anchor content should be non-empty"
        );
        assert!(
            budget.truncated,
            "expected truncation when first chunk exceeds max_chars"
        );
    }

    #[tokio::test]
    async fn map_works_without_index_and_has_no_side_effects() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let root_display = root.to_string_lossy().to_string();

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src").join("main.rs"),
            "fn main() { println!(\"hi\"); }\n",
        )
        .unwrap();

        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs").join("README.md"), "# Hello\n").unwrap();

        let context_dir = context_vector_store::context_dir_for_project_root(root);
        assert!(
            !context_dir.exists()
                && !root.join(".context").exists()
                && !root.join(".context-finder").exists()
        );

        let result = compute_map_result(root, &root_display, 1, 20, 0)
            .await
            .unwrap();
        assert_eq!(result.total_files, Some(2));
        assert!(result.total_chunks.unwrap_or(0) > 0);
        assert!(result.directories.iter().any(|d| d.path == "src"));
        assert!(result.directories.iter().any(|d| d.path == "docs"));
        assert!(!result.truncated);
        assert!(result.next_cursor.is_none());

        // `map` must not create indexes/corpus.
        assert!(
            !context_dir.exists()
                && !root.join(".context").exists()
                && !root.join(".context-finder").exists()
        );
    }

    #[tokio::test]
    async fn list_files_works_without_index_and_is_bounded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let root_display = root.to_string_lossy().to_string();

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();

        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs").join("README.md"), "# Hello\n").unwrap();

        std::fs::write(root.join("README.md"), "Root\n").unwrap();

        let context_dir = context_vector_store::context_dir_for_project_root(root);
        assert!(
            !context_dir.exists()
                && !root.join(".context").exists()
                && !root.join(".context-finder").exists()
        );

        let result = compute_list_files_result(root, &root_display, None, 50, 20_000, false, None)
            .await
            .unwrap();
        assert_eq!(result.source.as_deref(), Some("filesystem"));
        assert!(result.files.contains(&"src/main.rs".to_string()));
        assert!(result.files.contains(&"docs/README.md".to_string()));
        assert!(result.files.contains(&"README.md".to_string()));
        assert!(!result.truncated);
        assert!(result.next_cursor.is_none());

        let filtered =
            compute_list_files_result(root, &root_display, Some("docs"), 50, 20_000, false, None)
                .await
                .unwrap();
        assert_eq!(filtered.files, vec!["docs/README.md".to_string()]);
        assert!(!filtered.truncated);
        assert!(filtered.next_cursor.is_none());

        let globbed =
            compute_list_files_result(root, &root_display, Some("src/*"), 50, 20_000, false, None)
                .await
                .unwrap();
        assert_eq!(globbed.files, vec!["src/main.rs".to_string()]);
        assert!(!globbed.truncated);
        assert!(globbed.next_cursor.is_none());

        let limited = compute_list_files_result(root, &root_display, None, 1, 20_000, false, None)
            .await
            .unwrap();
        assert!(limited.truncated);
        assert_eq!(limited.truncation, Some(ListFilesTruncation::MaxItems));
        assert_eq!(limited.files.len(), 1);
        assert!(limited.next_cursor.is_some());

        let tiny = compute_list_files_result(root, &root_display, None, 50, 3, false, None)
            .await
            .unwrap();
        assert!(tiny.truncated);
        assert_eq!(tiny.truncation, Some(ListFilesTruncation::MaxChars));
        assert!(tiny.next_cursor.is_some());

        assert!(
            !context_dir.exists()
                && !root.join(".context").exists()
                && !root.join(".context-finder").exists()
        );
    }

    #[test]
    fn batch_prepare_item_input_injects_max_chars_for_ls() {
        let input = serde_json::json!({});
        let prepared = prepare_item_input(input, Some("/root"), BatchToolName::Ls, 5_000);

        let obj = prepared.as_object().expect("prepared input must be object");
        assert_eq!(obj.get("path").and_then(|v| v.as_str()), Some("/root"));
        assert!(
            obj.get("max_chars").is_some(),
            "expected max_chars to be injected for ls"
        );
    }

    #[test]
    fn batch_prepare_item_input_injects_max_chars_for_rg() {
        let input = serde_json::json!({});
        let prepared = prepare_item_input(input, Some("/root"), BatchToolName::Rg, 5_000);

        let obj = prepared.as_object().expect("prepared input must be object");
        assert_eq!(obj.get("path").and_then(|v| v.as_str()), Some("/root"));
        assert!(
            obj.get("max_chars").is_some(),
            "expected max_chars to be injected for rg"
        );
    }

    #[tokio::test]
    async fn doctor_manifest_parsing_reports_missing_assets() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let model_dir = tmp.path().join("models");
        std::fs::create_dir_all(&model_dir).unwrap();

        std::fs::write(
            model_dir.join("manifest.json"),
            r#"{"schema_version":1,"models":[{"id":"m1","assets":[{"path":"m1/model.onnx"}]}]}"#,
        )
        .unwrap();

        let (exists, models) = load_model_statuses(&model_dir).await.unwrap();
        assert!(exists);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "m1");
        assert!(!models[0].installed);
        assert_eq!(models[0].missing_assets, vec!["m1/model.onnx"]);
    }

    #[tokio::test]
    async fn doctor_drift_helpers_detect_missing_and_extra_chunks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let corpus_path = tmp.path().join("corpus.json");
        let index_path = tmp.path().join("index.json");

        let mut corpus = ChunkCorpus::new();
        corpus.set_file_chunks(
            "a.rs".to_string(),
            vec![context_code_chunker::CodeChunk::new(
                "a.rs".to_string(),
                1,
                2,
                "alpha".to_string(),
                ChunkMetadata::default(),
            )],
        );
        corpus.set_file_chunks(
            "c.rs".to_string(),
            vec![context_code_chunker::CodeChunk::new(
                "c.rs".to_string(),
                10,
                12,
                "gamma".to_string(),
                ChunkMetadata::default(),
            )],
        );
        corpus.save(&corpus_path).await.unwrap();

        // Index contains one correct chunk id (a.rs:1:2) and one extra (b.rs:1:1),
        // while missing c.rs:10:12.
        std::fs::write(
            &index_path,
            r#"{"schema_version":3,"dimension":384,"next_id":2,"id_map":{"0":"a.rs:1:2","1":"b.rs:1:1"},"vectors":{}}"#,
        )
        .unwrap();

        let corpus_ids = load_corpus_chunk_ids(&corpus_path).await.unwrap();
        let index_ids = load_index_chunk_ids(&index_path).await.unwrap();

        assert_eq!(corpus_ids.len(), 2);
        assert_eq!(index_ids.len(), 2);
        assert_eq!(corpus_ids.difference(&index_ids).count(), 1);
        assert_eq!(index_ids.difference(&corpus_ids).count(), 1);
    }

    fn mk_chunk(
        file_path: &str,
        start_line: usize,
        content: &str,
    ) -> context_code_chunker::CodeChunk {
        context_code_chunker::CodeChunk::new(
            file_path.to_string(),
            start_line,
            start_line + content.lines().count().saturating_sub(1),
            content.to_string(),
            ChunkMetadata::default(),
        )
    }

    #[test]
    fn prepare_excludes_docs_when_disabled() {
        let primary_code = SearchResult {
            id: "src/main.rs:1:1".to_string(),
            chunk: mk_chunk("src/main.rs", 1, "fn main() {}"),
            score: 0.9,
        };
        let primary_docs = SearchResult {
            id: "docs/readme.md:1:1".to_string(),
            chunk: mk_chunk("docs/readme.md", 1, "# docs"),
            score: 1.0,
        };

        let related_docs = RelatedContext {
            chunk: mk_chunk("docs/guide.md", 1, "# guide"),
            relationship_path: vec!["Calls".to_string()],
            distance: 1,
            relevance_score: 0.5,
        };
        let related_code = RelatedContext {
            chunk: mk_chunk("src/lib.rs", 10, "pub fn f() {}"),
            relationship_path: vec!["Calls".to_string()],
            distance: 1,
            relevance_score: 0.6,
        };

        let enriched = vec![
            EnrichedResult {
                primary: primary_docs,
                related: Vec::new(),
                total_lines: 1,
                strategy: context_graph::AssemblyStrategy::Extended,
            },
            EnrichedResult {
                primary: primary_code,
                related: vec![related_docs, related_code],
                total_lines: 1,
                strategy: context_graph::AssemblyStrategy::Extended,
            },
        ];

        let prepared = prepare_context_pack_enriched(enriched, 10, false, false);
        let files: Vec<&str> = prepared
            .iter()
            .map(|er| er.primary.chunk.file_path.as_str())
            .collect();
        assert_eq!(files, vec!["src/main.rs"]);

        let related_files: Vec<&str> = prepared[0]
            .related
            .iter()
            .map(|rc| rc.chunk.file_path.as_str())
            .collect();
        assert_eq!(related_files, vec!["src/lib.rs"]);
    }

    #[test]
    fn prepare_prefers_code_over_docs_when_enabled() {
        let primary_code = SearchResult {
            id: "src/main.rs:1:1".to_string(),
            chunk: mk_chunk("src/main.rs", 1, "fn main() {}"),
            score: 0.9,
        };
        let primary_docs = SearchResult {
            id: "docs/readme.md:1:1".to_string(),
            chunk: mk_chunk("docs/readme.md", 1, "# docs"),
            score: 1.0,
        };

        let enriched = vec![
            EnrichedResult {
                primary: primary_docs,
                related: Vec::new(),
                total_lines: 1,
                strategy: context_graph::AssemblyStrategy::Extended,
            },
            EnrichedResult {
                primary: primary_code,
                related: Vec::new(),
                total_lines: 1,
                strategy: context_graph::AssemblyStrategy::Extended,
            },
        ];

        let prepared = prepare_context_pack_enriched(enriched, 10, true, true);
        let files: Vec<&str> = prepared
            .iter()
            .map(|er| er.primary.chunk.file_path.as_str())
            .collect();
        assert_eq!(files, vec!["src/main.rs", "docs/readme.md"]);
    }

    #[test]
    fn focus_related_prefers_query_hits_over_raw_relevance() {
        let related_miss = RelatedContext {
            chunk: mk_chunk("src/miss.rs", 1, "fn unrelated() {}"),
            relationship_path: vec!["Calls".to_string()],
            distance: 1,
            relevance_score: 100.0,
        };
        let related_hit = RelatedContext {
            chunk: mk_chunk("src/hit.rs", 1, "fn target() {}"),
            relationship_path: vec!["Calls".to_string()],
            distance: 1,
            relevance_score: 0.1,
        };

        let query_tokens = vec!["target".to_string()];
        let prepared = prepare_related_contexts(
            vec![related_miss, related_hit],
            RelatedMode::Focus,
            &query_tokens,
        );
        assert_eq!(prepared[0].chunk.file_path, "src/hit.rs");
    }

    #[test]
    fn canonicalize_root_prefers_git_root_for_file_hint() {
        let dir = tempdir().expect("temp dir");
        std::fs::create_dir(dir.path().join(".git")).expect("create .git");

        let nested = dir.path().join("sub").join("inner");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        let file = nested.join("main.rs");
        std::fs::write(&file, "fn main() {}\n").expect("write file");

        let resolved = canonicalize_root_path(&file).expect("canonicalize root");
        assert_eq!(resolved, dir.path().canonicalize().expect("canonical root"));
    }

    #[test]
    fn root_path_from_mcp_uri_parses_file_uri() {
        let out = root_path_from_mcp_uri("file:///tmp/foo%20bar").expect("parse file uri");
        assert_eq!(out, PathBuf::from("/tmp/foo bar"));
    }
}
