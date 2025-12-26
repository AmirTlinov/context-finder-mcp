//! MCP tool dispatch for Context Finder
//!
//! Provides semantic code search capabilities to AI agents via MCP protocol.

use super::batch::{
    compute_used_chars, extract_path_from_input, parse_tool_result_as_json, prepare_item_input,
    push_item_or_truncate, resolve_batch_refs,
};
use super::cursor::{decode_cursor, encode_cursor, CURSOR_VERSION};
use super::file_slice::compute_file_slice_result;
use super::grep_context::{compute_grep_context_result, GrepContextComputeOptions};
use super::list_files::{compute_list_files_result, decode_list_files_cursor};
use super::map::{compute_map_result, decode_map_cursor};
use super::paths::normalize_relative_path;
use super::repo_onboarding_pack::compute_repo_onboarding_pack_result;
use super::schemas::batch::{
    BatchBudget, BatchItemResult, BatchItemStatus, BatchRequest, BatchResult, BatchToolName,
};
use super::schemas::context::{ContextHit, ContextRequest, ContextResult, RelatedCode};
use super::schemas::context_pack::ContextPackRequest;
use super::schemas::doctor::{
    DoctorEnvResult, DoctorIndexDrift, DoctorModelStatus, DoctorProjectResult, DoctorRequest,
    DoctorResult,
};
use super::schemas::explain::{ExplainRequest, ExplainResult};
use super::schemas::file_slice::{FileSliceCursorV1, FileSliceRequest};
use super::schemas::grep_context::{GrepContextCursorV1, GrepContextRequest};
use super::schemas::impact::{ImpactRequest, ImpactResult, SymbolLocation, UsageInfo};
use super::schemas::index::{IndexRequest, IndexResult};
use super::schemas::list_files::ListFilesRequest;
#[cfg(test)]
use super::schemas::list_files::ListFilesTruncation;
use super::schemas::map::MapRequest;
use super::schemas::overview::{
    GraphStats, KeyTypeInfo, LayerInfo, OverviewRequest, OverviewResult, ProjectInfo,
};
use super::schemas::read_pack::{
    ReadPackBudget, ReadPackIntent, ReadPackNextAction, ReadPackRequest, ReadPackResult,
    ReadPackSection, ReadPackTruncation,
};
use super::schemas::repo_onboarding_pack::RepoOnboardingPackRequest;
use super::schemas::search::{SearchRequest, SearchResult};
use super::schemas::text_search::{
    TextSearchCursorModeV1, TextSearchCursorV1, TextSearchMatch, TextSearchRequest,
    TextSearchResult,
};
use super::schemas::trace::{TraceRequest, TraceResult, TraceStep};
use super::util::{path_has_extension_ignore_ascii_case, unix_ms};
use crate::runtime_env;
use anyhow::{Context as AnyhowContext, Result};
use context_graph::{
    build_graph_docs, CodeGraph, ContextAssembler, GraphDocConfig, GraphEdge, GraphLanguage,
    GraphNode, RelationshipType, Symbol, GRAPH_DOC_VERSION,
};
use context_indexer::{
    assess_staleness, compute_project_watermark, read_index_watermark, FileScanner, IndexSnapshot,
    IndexState, IndexerError, PersistedIndexWatermark, ReindexAttempt, ReindexResult, ToolMeta,
    INDEX_STATE_SCHEMA_VERSION,
};
use context_search::{
    ContextPackBudget, ContextPackItem, ContextPackOutput, MultiModelContextSearch,
    MultiModelHybridSearch, QueryClassifier, QueryType, SearchProfile, CONTEXT_PACK_VERSION,
};
use context_vector_store::{
    classify_path_kind, corpus_path_for_project_root, current_model_id, ChunkCorpus, DocumentKind,
    GraphNodeDoc, GraphNodeStore, GraphNodeStoreMeta, QueryKind, VectorIndex,
};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Mutex;

/// Context Finder MCP Service
#[derive(Clone)]
pub struct ContextFinderService {
    /// Search profile
    profile: SearchProfile,
    /// Tool router
    tool_router: ToolRouter<Self>,
    /// Shared cache state (per-process)
    state: Arc<ServiceState>,
}

impl ContextFinderService {
    pub fn new() -> Self {
        Self {
            profile: load_profile_from_env(),
            tool_router: Self::tool_router(),
            state: Arc::new(ServiceState::new()),
        }
    }

    pub(super) async fn resolve_root(
        &self,
        raw_path: Option<&str>,
    ) -> Result<(PathBuf, String), String> {
        let (root, root_display) = self.state.resolve_root(raw_path).await?;
        Self::touch_daemon_best_effort(&root);
        Ok((root, root_display))
    }
}

const DEFAULT_AUTO_INDEX_BUDGET_MS: u64 = 3_000;
const MIN_AUTO_INDEX_BUDGET_MS: u64 = 100;
const MAX_AUTO_INDEX_BUDGET_MS: u64 = 120_000;

#[derive(Clone, Copy, Debug)]
pub(in crate::tools::dispatch) struct AutoIndexPolicy {
    enabled: bool,
    budget_ms: u64,
}

impl AutoIndexPolicy {
    pub(in crate::tools::dispatch) fn from_request(
        auto_index: Option<bool>,
        auto_index_budget_ms: Option<u64>,
    ) -> Self {
        let enabled = auto_index.unwrap_or(true);
        let budget_ms = auto_index_budget_ms
            .unwrap_or(DEFAULT_AUTO_INDEX_BUDGET_MS)
            .clamp(MIN_AUTO_INDEX_BUDGET_MS, MAX_AUTO_INDEX_BUDGET_MS);
        Self { enabled, budget_ms }
    }
}

fn load_profile_from_env() -> SearchProfile {
    let profile_name = std::env::var("CONTEXT_FINDER_PROFILE")
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
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some("Context Finder provides semantic code search for AI agents. Use 'map' to explore project structure, 'search' for semantic queries, 'context' for search with related code, 'index' to index new projects, and 'doctor' to diagnose model/GPU/index configuration.".into()),
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
        match gather_index_state(root, &self.profile).await {
            Ok(index_state) => ToolMeta {
                index_state: Some(index_state),
            },
            Err(err) => {
                log::debug!("index_state unavailable for {}: {err:#}", root.display());
                ToolMeta { index_state: None }
            }
        }
    }

    async fn prepare_semantic_engine(
        &self,
        root: &Path,
        policy: AutoIndexPolicy,
    ) -> Result<(EngineLock, ToolMeta)> {
        let mut index_state = gather_index_state(root, &self.profile).await?;
        let mut attempt: Option<ReindexAttempt> = None;

        if policy.enabled && (index_state.stale || !index_state.index.exists) {
            let reindex = self.attempt_reindex(root, policy.budget_ms).await;
            attempt = Some(reindex.clone());
            if let Ok(refreshed) = gather_index_state(root, &self.profile).await {
                index_state = refreshed;
            }
            index_state.reindex = Some(reindex);
        }

        if !index_state.index.exists {
            return Err(anyhow::anyhow!(missing_index_message(
                &index_state,
                attempt.as_ref()
            )));
        }

        let engine = self.lock_engine(root).await?;
        let meta = ToolMeta {
            index_state: Some(index_state),
        };
        Ok((engine, meta))
    }

    async fn lock_engine(&self, root: &Path) -> Result<EngineLock> {
        Self::touch_daemon_best_effort(root);

        let handle = self.state.engine_handle(root).await;
        let mut slot = handle.lock_owned().await;

        let signature = compute_engine_signature(root, &self.profile).await?;
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

    fn touch_daemon_best_effort(root: &Path) {
        let disable = std::env::var("CONTEXT_FINDER_DISABLE_DAEMON")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if disable {
            return;
        }

        let root = root.to_path_buf();
        if tokio::runtime::Handle::try_current().is_err() {
            log::debug!("daemon touch skipped (no runtime)");
            return;
        }
        tokio::spawn(async move {
            if let Err(err) = crate::daemon::touch(&root).await {
                log::debug!("daemon touch failed: {err:#}");
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
    root.join(".context-finder")
        .join("indexes")
        .join(model_id_dir_name(&model_id))
        .join("graph_nodes.json")
}

fn index_path_for_model(root: &Path, model_id: &str) -> PathBuf {
    root.join(".context-finder")
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
    let mut message = format!("Index not found at {path}. Run 'context-finder index' first.");
    if let Some(attempt) = attempt {
        message.push_str(" Auto-index attempt: ");
        message.push_str(&format_reindex_attempt(attempt));
        message.push('.');
    }
    message
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

fn semantic_model_roster(profile: &SearchProfile) -> Vec<String> {
    let experts = profile.experts();
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for kind in [
        QueryKind::Identifier,
        QueryKind::Path,
        QueryKind::Conceptual,
    ] {
        for model_id in experts.semantic_models(kind) {
            if seen.insert(model_id.clone()) {
                out.push(model_id.clone());
            }
        }
    }

    out
}

async fn load_semantic_indexes(
    root: &Path,
    profile: &SearchProfile,
) -> Result<Vec<(String, VectorIndex)>> {
    let default_model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());

    let mut requested: Vec<String> = Vec::new();
    requested.push(default_model_id.clone());
    requested.extend(semantic_model_roster(profile));

    let mut sources = Vec::new();
    let mut seen = HashSet::new();
    for model_id in requested {
        if !seen.insert(model_id.clone()) {
            continue;
        }
        let path = index_path_for_model(root, &model_id);
        if !path.exists() {
            continue;
        }
        let index = VectorIndex::load(&path)
            .await
            .with_context(|| format!("Failed to load index {}", path.display()))?;
        sources.push((model_id, index));
    }

    if sources.is_empty() {
        anyhow::bail!("No semantic indices available (run 'context-finder index' first)");
    }

    Ok(sources)
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

type EngineHandle = Arc<Mutex<EngineSlot>>;

struct ServiceState {
    engines: Mutex<EngineCache>,
    session: Mutex<SessionDefaults>,
}

impl ServiceState {
    fn new() -> Self {
        Self {
            engines: Mutex::new(EngineCache::new(ENGINE_CACHE_CAPACITY)),
            session: Mutex::new(SessionDefaults::default()),
        }
    }

    async fn engine_handle(&self, root: &Path) -> EngineHandle {
        let mut cache = self.engines.lock().await;
        cache.get_or_insert(root)
    }

    async fn resolve_root(&self, raw_path: Option<&str>) -> Result<(PathBuf, String), String> {
        if let Some(raw) = trimmed_non_empty(raw_path) {
            let root = canonicalize_root(raw).map_err(|err| format!("Invalid path: {err}"))?;
            let root_display = root.to_string_lossy().to_string();
            let mut session = self.session.lock().await;
            session.root = Some(root.clone());
            session.root_display = Some(root_display.clone());
            return Ok((root, root_display));
        }

        if let Some((root, root_display)) = self.session.lock().await.clone_root() {
            return Ok((root, root_display));
        }

        if let Some((var, value)) = env_root_override() {
            let root = canonicalize_root(&value)
                .map_err(|err| format!("Invalid path from {var}: {err}"))?;
            let root_display = root.to_string_lossy().to_string();
            let mut session = self.session.lock().await;
            session.root = Some(root.clone());
            session.root_display = Some(root_display.clone());
            return Ok((root, root_display));
        }

        let cwd = env::current_dir()
            .map_err(|err| format!("Failed to determine current directory: {err}"))?;
        let candidate = find_git_root(&cwd).unwrap_or(cwd);
        let root =
            canonicalize_root_path(&candidate).map_err(|err| format!("Invalid path: {err}"))?;
        let root_display = root.to_string_lossy().to_string();
        let mut session = self.session.lock().await;
        session.root = Some(root.clone());
        session.root_display = Some(root_display.clone());
        Ok((root, root_display))
    }
}

#[derive(Default)]
struct SessionDefaults {
    root: Option<PathBuf>,
    root_display: Option<String>,
}

impl SessionDefaults {
    fn clone_root(&self) -> Option<(PathBuf, String)> {
        Some((self.root.clone()?, self.root_display.clone()?))
    }
}

fn trimmed_non_empty(input: Option<&str>) -> Option<&str> {
    input.map(str::trim).filter(|value| !value.is_empty())
}

fn env_root_override() -> Option<(String, String)> {
    for key in ["CONTEXT_FINDER_ROOT", "CONTEXT_FINDER_PROJECT_ROOT"] {
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
    path.canonicalize().map_err(|err| err.to_string())
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(PathBuf::from)
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
    available_models: Vec<String>,
    canonical_index_mtime: SystemTime,
    graph_language: Option<GraphLanguage>,
}

impl ProjectEngine {
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
            path: project_root
                .join(".context-finder")
                .join("graph_cache.json"),
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

async fn compute_engine_signature(root: &Path, profile: &SearchProfile) -> Result<EngineSignature> {
    let corpus_path = corpus_path_for_project_root(root);
    let corpus_mtime_ms = tokio::fs::metadata(&corpus_path)
        .await
        .and_then(|m| m.modified())
        .ok()
        .map(unix_ms);

    let default_model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    let mut models = Vec::new();
    models.push(default_model_id);
    models.extend(semantic_model_roster(profile));
    models.sort();
    models.dedup();

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
    let sources = load_semantic_indexes(root, profile).await?;
    let mut available_models: Vec<String> = sources.iter().map(|(id, _)| id.clone()).collect();
    available_models.sort();

    let canonical_model_id = available_models
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("No semantic indices available"))?;
    let canonical_index_path = index_path_for_model(root, &canonical_model_id);
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
        available_models,
        canonical_index_mtime,
        graph_language: None,
    })
}

// ============================================================================
// Tool Input/Output Schemas
// ============================================================================

#[derive(Debug, Deserialize)]
struct ModelManifestFile {
    models: Vec<ModelManifestModel>,
}

#[derive(Debug, Deserialize)]
struct ModelManifestModel {
    id: String,
    assets: Vec<ModelManifestAsset>,
}

#[derive(Debug, Deserialize)]
struct ModelManifestAsset {
    path: String,
}

fn validate_relative_model_asset_path(path: &Path) -> Result<()> {
    let mut has_component = false;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                anyhow::bail!("asset path must be relative");
            }
            Component::ParentDir => {
                anyhow::bail!("asset path must not contain '..'");
            }
            Component::CurDir => {}
            Component::Normal(_) => {
                has_component = true;
            }
        }
    }
    if !has_component {
        anyhow::bail!("asset path is empty");
    }
    Ok(())
}

fn safe_join_model_asset_path(model_dir: &Path, asset_path: &str) -> Result<PathBuf> {
    let rel = Path::new(asset_path);
    validate_relative_model_asset_path(rel)
        .with_context(|| format!("Invalid model asset path '{asset_path}'"))?;
    Ok(model_dir.join(rel))
}

#[cfg(test)]
mod model_asset_path_tests {
    use super::*;

    #[test]
    fn safe_join_rejects_traversal_and_absolute_paths() {
        let base = Path::new("models");
        assert!(safe_join_model_asset_path(base, "../escape").is_err());
        assert!(safe_join_model_asset_path(base, "m1/../escape").is_err());
        assert!(safe_join_model_asset_path(base, "").is_err());

        #[cfg(unix)]
        assert!(safe_join_model_asset_path(base, "/etc/passwd").is_err());
    }

    #[test]
    fn safe_join_accepts_normal_relative_paths() {
        let base = Path::new("models");
        let path = safe_join_model_asset_path(base, "m1/model.onnx").expect("valid path");
        assert!(path.starts_with(base));
    }
}

#[derive(Debug, Deserialize)]
struct IndexIdMapOnly {
    #[serde(default)]
    schema_version: Option<u32>,
    #[serde(default)]
    id_map: HashMap<usize, String>,
}

async fn load_model_statuses(model_dir: &Path) -> Result<(bool, Vec<DoctorModelStatus>)> {
    let manifest_path = model_dir.join("manifest.json");
    if !manifest_path.exists() {
        return Ok((false, Vec::new()));
    }

    let bytes = tokio::fs::read(&manifest_path)
        .await
        .with_context(|| format!("Failed to read model manifest {}", manifest_path.display()))?;
    let parsed: ModelManifestFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("Failed to parse model manifest {}", manifest_path.display()))?;

    let mut statuses = Vec::new();
    for model in parsed.models {
        let mut missing = Vec::new();
        for asset in model.assets {
            let full = match safe_join_model_asset_path(model_dir, &asset.path) {
                Ok(path) => path,
                Err(err) => {
                    missing.push(format!("invalid_path: {} ({err})", asset.path));
                    continue;
                }
            };
            if !full.exists() {
                missing.push(asset.path);
            }
        }
        let installed = missing.is_empty();
        statuses.push(DoctorModelStatus {
            id: model.id,
            installed,
            missing_assets: missing,
        });
    }
    Ok((true, statuses))
}

async fn load_corpus_chunk_ids(corpus_path: &Path) -> Result<HashSet<String>> {
    let corpus = ChunkCorpus::load(corpus_path).await?;
    let mut ids = HashSet::new();
    for chunks in corpus.files().values() {
        for chunk in chunks {
            ids.insert(format!(
                "{}:{}:{}",
                chunk.file_path, chunk.start_line, chunk.end_line
            ));
        }
    }
    Ok(ids)
}

async fn load_index_chunk_ids(index_path: &Path) -> Result<HashSet<String>> {
    let bytes = tokio::fs::read(index_path)
        .await
        .with_context(|| format!("Failed to read index {}", index_path.display()))?;
    let parsed: IndexIdMapOnly = serde_json::from_slice(&bytes)
        .with_context(|| format!("Failed to parse index {}", index_path.display()))?;
    // schema_version is tracked for diagnostics, but chunk id extraction relies on id_map values.
    let _ = parsed.schema_version.unwrap_or(1);
    Ok(parsed.id_map.into_values().collect())
}

fn chunk_id_file_path(chunk_id: &str) -> Option<String> {
    let mut parts = chunk_id.rsplitn(3, ':');
    let _end = parts.next()?;
    let _start = parts.next()?;
    Some(parts.next()?.to_string())
}

fn sample_file_paths<'a, I>(chunk_ids: I, limit: usize) -> Vec<String>
where
    I: Iterator<Item = &'a String>,
{
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for id in chunk_ids {
        if out.len() >= limit {
            break;
        }
        let Some(file) = chunk_id_file_path(id) else {
            continue;
        };
        if seen.insert(file.clone()) {
            out.push(file);
        }
    }
    out
}

// ============================================================================
// Tool Implementations
// ============================================================================

mod router;

#[tool_router]
impl ContextFinderService {
    /// Get project structure overview
    #[tool(
        description = "Get project structure overview with directories, files, and top symbols. Use this first to understand a new codebase."
    )]
    pub async fn map(
        &self,
        Parameters(request): Parameters<MapRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::map::map(self, request).await
    }

    /// Repo onboarding pack (map + key docs slices + next actions).
    #[tool(
        description = "Build a repo onboarding pack: map + key docs (via file slices) + next actions. Returns a single bounded JSON response for fast project adoption."
    )]
    pub async fn repo_onboarding_pack(
        &self,
        Parameters(request): Parameters<RepoOnboardingPackRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::repo_onboarding_pack::repo_onboarding_pack(self, request).await
    }

    /// Bounded exact text search (literal substring), as a safe `rg` replacement.
    #[tool(
        description = "Search for an exact text pattern in project files with bounded output (rg-like, but safe for agent context). Uses corpus if available, otherwise scans files without side effects."
    )]
    pub async fn text_search(
        &self,
        Parameters(request): Parameters<TextSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::text_search::text_search(self, request).await
    }

    /// Read a bounded slice of a file within the project root (safe file access for agents).
    #[tool(
        description = "Read a bounded slice of a file (by line) within the project root. Safe replacement for ad-hoc `cat/sed` reads; enforces max_lines/max_chars and prevents path traversal."
    )]
    pub async fn file_slice(
        &self,
        Parameters(request): Parameters<FileSliceRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::file_slice::file_slice(self, &request).await
    }

    /// Build a one-call semantic reading pack (file slice / grep context / context pack / onboarding).
    #[tool(
        description = "One-call semantic reading pack. A cognitive facade over file_slice/grep_context/context_pack/repo_onboarding_pack: returns the most relevant bounded slice(s) plus continuation cursors and next actions."
    )]
    pub async fn read_pack(
        &self,
        Parameters(request): Parameters<ReadPackRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::read_pack::read_pack(self, request).await
    }

    /// List project files within the project root (safe file enumeration for agents).
    #[tool(
        description = "List project file paths (relative to project root). Safe replacement for `ls/find/rg --files`; supports glob/substring filtering and bounded output."
    )]
    pub async fn list_files(
        &self,
        Parameters(request): Parameters<ListFilesRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::list_files::list_files(self, request).await
    }

    /// Regex search with merged context hunks (grep-like).
    #[tool(
        description = "Search project files with a regex and return merged context hunks (N lines before/after). Designed to replace `rg -C/-A/-B` plus multiple file_slice calls with a single bounded response."
    )]
    pub async fn grep_context(
        &self,
        Parameters(request): Parameters<GrepContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::grep_context::grep_context(self, request).await
    }

    /// Execute multiple Context Finder tools in a single call (agent-friendly batch).
    #[tool(
        description = "Execute multiple Context Finder tools in one call. Returns a single bounded JSON result with per-item status (partial success) and a global max_chars budget."
    )]
    pub async fn batch(
        &self,
        Parameters(request): Parameters<BatchRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::batch::batch(self, request).await
    }

    /// Diagnose model/GPU/index configuration
    #[tool(
        description = "Show diagnostics for model directory, CUDA/ORT runtime, and per-project index/corpus status. Use this when something fails (e.g., GPU provider missing)."
    )]
    pub async fn doctor(
        &self,
        Parameters(request): Parameters<DoctorRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::doctor::doctor(self, request).await
    }

    /// Semantic code search
    #[tool(
        description = "Search for code using natural language. Returns relevant code snippets with file locations and symbols."
    )]
    pub async fn search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::search::search(self, request).await
    }

    /// Search with graph context
    #[tool(
        description = "Search for code with automatic graph-based context. Returns code plus related functions/types through call graphs and dependencies. Best for understanding how code connects."
    )]
    pub async fn context(
        &self,
        Parameters(request): Parameters<ContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::context::context(self, request).await
    }

    /// Build a bounded context pack for agents (single-call context).
    #[tool(
        description = "Build a bounded `context_pack` JSON for a query: primary hits + graph-related halo, under a strict character budget. Intended as the single-call payload for AI agents."
    )]
    pub async fn context_pack(
        &self,
        Parameters(request): Parameters<ContextPackRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::context_pack::context_pack(self, request).await
    }

    /// Index a project
    #[tool(
        description = "Index a project directory for semantic search. Required before using search/context tools on a new project."
    )]
    pub async fn index(
        &self,
        Parameters(request): Parameters<IndexRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::index::index(self, request).await
    }

    /// Find all usages of a symbol (impact analysis)
    #[tool(
        description = "Find all places where a symbol is used. Essential for refactoring - shows direct usages, transitive dependencies, and related tests."
    )]
    pub async fn impact(
        &self,
        Parameters(request): Parameters<ImpactRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::impact::impact(self, request).await
    }

    /// Trace call path between two symbols
    #[tool(
        description = "Show call chain from one symbol to another. Essential for understanding code flow and debugging."
    )]
    pub async fn trace(
        &self,
        Parameters(request): Parameters<TraceRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::trace::trace(self, request).await
    }

    /// Deep dive into a symbol
    #[tool(
        description = "Get complete information about a symbol: definition, dependencies, dependents, tests, and documentation."
    )]
    pub async fn explain(
        &self,
        Parameters(request): Parameters<ExplainRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::explain::explain(self, request).await
    }

    /// Project architecture overview
    #[tool(
        description = "Get project architecture snapshot: layers, entry points, key types, and graph statistics. Use this first to understand a new codebase."
    )]
    pub async fn overview(
        &self,
        Parameters(request): Parameters<OverviewRequest>,
    ) -> Result<CallToolResult, McpError> {
        router::overview::overview(self, request).await
    }
}

fn finalize_read_pack_budget(result: &mut ReadPackResult) -> anyhow::Result<()> {
    let mut used = 0usize;
    for _ in 0..8 {
        result.budget.used_chars = used;
        let raw = serde_json::to_string(result)?;
        let next = raw.chars().count();
        if next == used {
            result.budget.used_chars = next;
            return Ok(());
        }
        used = next;
    }

    result.budget.used_chars = used;
    Ok(())
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

fn pack_enriched_results(
    profile: &SearchProfile,
    enriched: Vec<context_search::EnrichedResult>,
    max_chars: usize,
    max_related_per_primary: usize,
    related_mode: RelatedMode,
    query_tokens: &[String],
) -> (Vec<ContextPackItem>, ContextPackBudget) {
    let mut used_chars = 0usize;
    let mut truncated = false;
    let mut dropped_items = 0usize;

    let mut items: Vec<ContextPackItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for er in enriched {
        let primary = er.primary;
        if !seen.insert(primary.id.clone()) {
            continue;
        }

        let primary_item = build_primary_item(primary);
        let cost = estimate_item_chars(&primary_item);
        if used_chars.saturating_add(cost) > max_chars {
            truncated = true;
            dropped_items += 1;
            break;
        }
        used_chars += cost;
        items.push(primary_item);

        let mut related = er.related;
        related.retain(|rc| !profile.is_rejected(&rc.chunk.file_path));
        let related = prepare_related_contexts(related, related_mode, query_tokens);

        let mut selected_related = 0usize;
        let mut per_relationship: HashMap<String, usize> = HashMap::new();
        for rc in related {
            if selected_related >= max_related_per_primary {
                break;
            }

            let kind = rc
                .relationship_path
                .first()
                .cloned()
                .unwrap_or_else(String::new);
            let cap = relationship_cap(&kind);
            let used = per_relationship.get(kind.as_str()).copied().unwrap_or(0);
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
                break;
            }
            used_chars += cost;
            items.push(item);
            *per_relationship.entry(kind).or_insert(0) += 1;
            selected_related += 1;
        }

        if truncated {
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
        score,
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
        score: rc.relevance_score,
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

        assert!(!root.join(".context-finder").exists());

        let result = compute_map_result(root, &root_display, 1, 20, 0)
            .await
            .unwrap();
        assert_eq!(result.total_files, 2);
        assert!(result.total_chunks > 0);
        assert!(result.directories.iter().any(|d| d.path == "src"));
        assert!(result.directories.iter().any(|d| d.path == "docs"));
        assert!(!result.truncated);
        assert!(result.next_cursor.is_none());

        // `map` must not create indexes/corpus.
        assert!(!root.join(".context-finder").exists());
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

        assert!(!root.join(".context-finder").exists());

        let result = compute_list_files_result(root, &root_display, None, 50, 20_000, None)
            .await
            .unwrap();
        assert_eq!(result.source, "filesystem");
        assert!(result.files.contains(&"src/main.rs".to_string()));
        assert!(result.files.contains(&"docs/README.md".to_string()));
        assert!(result.files.contains(&"README.md".to_string()));
        assert!(!result.truncated);
        assert!(result.next_cursor.is_none());

        let filtered =
            compute_list_files_result(root, &root_display, Some("docs"), 50, 20_000, None)
                .await
                .unwrap();
        assert_eq!(filtered.files, vec!["docs/README.md".to_string()]);
        assert!(!filtered.truncated);
        assert!(filtered.next_cursor.is_none());

        let globbed =
            compute_list_files_result(root, &root_display, Some("src/*"), 50, 20_000, None)
                .await
                .unwrap();
        assert_eq!(globbed.files, vec!["src/main.rs".to_string()]);
        assert!(!globbed.truncated);
        assert!(globbed.next_cursor.is_none());

        let limited = compute_list_files_result(root, &root_display, None, 1, 20_000, None)
            .await
            .unwrap();
        assert!(limited.truncated);
        assert_eq!(limited.truncation, Some(ListFilesTruncation::Limit));
        assert_eq!(limited.files.len(), 1);
        assert!(limited.next_cursor.is_some());

        let tiny = compute_list_files_result(root, &root_display, None, 50, 3, None)
            .await
            .unwrap();
        assert!(tiny.truncated);
        assert_eq!(tiny.truncation, Some(ListFilesTruncation::MaxChars));
        assert!(tiny.next_cursor.is_none());

        assert!(!root.join(".context-finder").exists());
    }

    #[test]
    fn batch_prepare_item_input_injects_max_chars_for_list_files() {
        let input = serde_json::json!({});
        let prepared = prepare_item_input(input, Some("/root"), BatchToolName::ListFiles, 5_000);

        let obj = prepared.as_object().expect("prepared input must be object");
        assert_eq!(obj.get("path").and_then(|v| v.as_str()), Some("/root"));
        assert!(
            obj.get("max_chars").is_some(),
            "expected max_chars to be injected for list_files"
        );
    }

    #[test]
    fn batch_prepare_item_input_injects_max_chars_for_grep_context() {
        let input = serde_json::json!({});
        let prepared = prepare_item_input(input, Some("/root"), BatchToolName::GrepContext, 5_000);

        let obj = prepared.as_object().expect("prepared input must be object");
        assert_eq!(obj.get("path").and_then(|v| v.as_str()), Some("/root"));
        assert!(
            obj.get("max_chars").is_some(),
            "expected max_chars to be injected for grep_context"
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
}
