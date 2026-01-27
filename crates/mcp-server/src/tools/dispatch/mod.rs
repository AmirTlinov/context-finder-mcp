//! MCP tool dispatch for Context
//!
//! Provides semantic code search capabilities to AI agents via MCP protocol.

use super::batch::{
    compute_used_chars, extract_path_from_input, parse_tool_result_as_json, prepare_item_input,
    push_item_or_truncate, resolve_batch_refs, trim_output_to_budget,
};
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
use super::schemas::root::{RootGetRequest, RootGetResult, RootSetRequest, RootSetResult};
use super::schemas::runbook_pack::RunbookPackRequest;
pub(super) use super::schemas::search::{SearchRequest, SearchResponse, SearchResult};
use super::schemas::text_search::TextSearchRequest;
use super::schemas::trace::{TraceRequest, TraceResult, TraceStep};
use super::schemas::worktree_pack::WorktreePackRequest;
use super::util::{path_has_extension_ignore_ascii_case, unix_ms};
use super::worktree_pack::{compute_worktree_pack_result, render_worktree_pack_block};
use anyhow::{Context as AnyhowContext, Result};
use context_graph::{
    build_graph_docs, CodeGraph, ContextAssembler, GraphDocConfig, GraphEdge, GraphLanguage,
    GraphNode, RelationshipType, Symbol, GRAPH_DOC_VERSION,
};
use context_indexer::{
    assess_staleness, compute_project_watermark, read_index_watermark, root_fingerprint,
    IndexSnapshot, IndexState, IndexerError, PersistedIndexWatermark, ReindexAttempt,
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
    QueryKind, VectorIndex,
};
use rmcp::model::{CallToolResult, Content};
use rmcp::ErrorData as McpError;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{Mutex, Notify};

mod budgets;
use budgets::{mcp_default_budgets, AutoIndexPolicy};
mod tool_router_hints;

mod service;

mod doctor_helpers;
use doctor_helpers::{
    load_corpus_chunk_ids, load_index_chunk_ids, load_model_statuses, sample_file_paths,
};

mod cursor_store;
use cursor_store::CursorStore;

mod root;
use root::SessionDefaults;

/// Context MCP Service
#[derive(Clone)]
pub struct ContextFinderService {
    /// Search profile
    profile: SearchProfile,
    /// Tool router
    tool_router: tool_router_hints::ToolRouterWithParamHints<Self>,
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

// Constructors and MCP `ServerHandler` implementation live in `service.rs`.

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
        let disable = std::env::var("CONTEXT_DISABLE_DAEMON")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if disable {
            return;
        }

        // In stub embedding mode we aim for deterministic, dependency-light behavior
        // (used heavily in CI and tests). The background daemon is a performance
        // optimization and can introduce nondeterminism / flakiness in constrained
        // environments, so we skip it.
        let stub = std::env::var("CONTEXT_EMBEDDING_MODE")
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
        let disable = std::env::var("CONTEXT_DISABLE_DAEMON")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if disable {
            return;
        }

        // In stub embedding mode we aim for deterministic, dependency-light behavior.
        // Background refresh is a performance optimization and can introduce nondeterminism,
        // so we skip it.
        let stub = std::env::var("CONTEXT_EMBEDDING_MODE")
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
    std::env::var("CONTEXT_ENGINE_SEMANTIC_INDEX_CAPACITY")
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
mod tests;
