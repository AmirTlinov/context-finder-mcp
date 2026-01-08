use crate::graph_cache::GraphCache;
use crate::metrics::MetricsExporter;
use anyhow::{Context as AnyhowContext, Result};
use context_graph::GraphLanguage;
use context_indexer::{IndexerHealth, StreamingIndexer, ProjectIndexer};
use context_search::{ContextSearch, HybridSearch};
use context_vector_store::{context_dir_for_project_root, VectorStore};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tonic::{Request, Response, Status};
use once_cell::sync::Lazy;
use context_vector_store::EmbeddingsSingleton;

pub mod proto {
    tonic::include_proto!("contextfinder");
}

use proto::context_finder_server::{ContextFinder, ContextFinderServer};
use proto::{
    HealthRequest, HealthResponse, RelatedChunk, SearchRequest, SearchResponse, SearchResult,
    TriggerIndexRequest, TriggerIndexResponse,
};

const REFRESH_LOG_WINDOW: Duration = Duration::from_secs(5);
const STALE_REINDEX_MS: u64 = 5 * 60 * 1000; // 5 minutes
const HEARTBEAT_CHECK_MS: u64 = 5 * 60 * 1000; // 5 minutes background stale check

#[derive(Default)]
struct RefreshLogState {
    last_reason: Option<String>,
    last_logged_at: Option<Instant>,
}

impl RefreshLogState {
    fn should_log(&mut self, reason: &str) -> bool {
        let now = Instant::now();
        let same_reason = self.last_reason.as_deref() == Some(reason);
        let within_window = self
            .last_logged_at
            .map(|ts| now.duration_since(ts) <= REFRESH_LOG_WINDOW)
            .unwrap_or(false);

        if same_reason && within_window {
            return false;
        }

        self.last_reason = Some(reason.to_string());
        self.last_logged_at = Some(now);
        true
    }
}

#[derive(Clone)]
pub struct DaemonConfig {
    pub project_root: PathBuf,
    pub context_depth: u8,
    pub graph_language: Option<GraphLanguage>,
    pub streaming_indexer: Option<StreamingIndexer>,
    pub graph_cache: GraphCache,
    pub health_log: Option<PathBuf>,
    pub metrics: Option<MetricsExporter>,
}

#[derive(Clone)]
pub struct ContextFinderService {
    config: DaemonConfig,
    shared: Arc<Mutex<ContextSearch>>,
    streaming_indexer: Option<StreamingIndexer>,
    _metrics: Option<MetricsExporter>,
    _health_log_handle: Option<Arc<JoinHandle<()>>>,
    _refresh_log: Arc<Mutex<RefreshLogState>>,
}

impl ContextFinderService {
    pub async fn new(config: DaemonConfig) -> Result<Self> {
        let search = build_context_search(&config).await?;
        let shared = Arc::new(Mutex::new(search));
        let streaming_indexer = config.streaming_indexer.clone();
        let metrics = config.metrics.clone();
        let mut health_log_handle: Option<Arc<JoinHandle<()>>> = None;
        let refresh_log = Arc::new(Mutex::new(RefreshLogState::default()));

        if let Some(streamer) = streaming_indexer.clone() {
            let mut rx = streamer.subscribe_updates();
            let shared_ref = Arc::clone(&shared);
            let cfg = config.clone();
            let refresh_log_handle = Arc::clone(&refresh_log);
            // Pre-warm embeddings once per daemon
            tokio::spawn(async move {
                let _ = EmbeddingsSingleton::instance().await;
            });
            tokio::spawn(async move {
                while let Ok(update) = rx.recv().await {
                    if !update.success {
                        continue;
                    }
                    if let Err(err) = reload_search(&shared_ref, &cfg).await {
                        log::error!("Failed to refresh context search: {err}");
                        continue;
                    }

                    let should_log = {
                        let mut guard = refresh_log_handle.lock().await;
                        guard.should_log(&update.reason)
                    };

                    if should_log {
                        log::info!(
                            "Context search refreshed after indexing ({})",
                            update.reason
                        );
                    } else {
                        log::debug!("Context search refresh deduplicated ({})", update.reason);
                    }
                }
            });
            if let Some(metrics) = metrics.clone() {
                metrics.update(Some(&streamer.health_snapshot()));
                let mut health_rx = streamer.health_stream();
                tokio::spawn(async move {
                    // periodic stale check
                    let mut interval =
                        tokio::time::interval(Duration::from_millis(HEARTBEAT_CHECK_MS));
                    loop {
                        tokio::select! {
                            changed = health_rx.changed() => {
                                if changed.is_err() { break; }
                                let snapshot = health_rx.borrow().clone();
                                metrics.update(Some(&snapshot));
                            }
                            _ = interval.tick() => {
                                let snapshot = health_rx.borrow().clone();
                                let age_ms = snapshot.last_success.and_then(|last| last.elapsed().ok()).map(|d| d.as_millis() as u64);
                                if age_ms.unwrap_or(STALE_REINDEX_MS + 1) > STALE_REINDEX_MS {
                                    let _ = streamer.trigger("stale-auto").await;
                                }
                            }
                        };
                    }
                });
            }

            if let Some(path) = config.health_log.clone() {
                let streamer_for_log = streamer.clone();
                let handle = tokio::spawn(async move {
                    if let Err(err) = health_log_loop(streamer_for_log, path).await {
                        log::error!("Health log writer stopped: {err}");
                    }
                });
                health_log_handle = Some(Arc::new(handle));
            }
        } else if let Some(metrics) = metrics.clone() {
            metrics.update(None);
            if config.health_log.is_some() {
                log::warn!("--health-log ignored: watcher disabled (--no-watch)");
            }
        } else if config.health_log.is_some() {
            log::warn!("--health-log ignored: watcher disabled (--no-watch)");
        }

        Ok(Self {
            config,
            shared,
            streaming_indexer,
            _metrics: metrics,
            _health_log_handle: health_log_handle,
            _refresh_log: refresh_log,
        })
    }

    pub fn into_service(self) -> ContextFinderServer<Self> {
        ContextFinderServer::new(self)
    }
}

#[tonic::async_trait]
impl ContextFinder for ContextFinderService {
    async fn search(
        &self,
        request: Request<SearchRequest>,
    ) -> Result<Response<SearchResponse>, Status> {
        let req = request.into_inner();
        if req.query.trim().is_empty() {
            return Err(Status::invalid_argument("query must not be empty"));
        }
        let limit = req.limit.max(1) as usize;

        let mut guard = self.shared.lock().await;
        let use_context = self.config.context_depth > 0 && guard.has_graph();

        if use_context {
            let strategy = match self.config.context_depth {
                0 | 1 => context_graph::AssemblyStrategy::Direct,
                2 => context_graph::AssemblyStrategy::Extended,
                3 => context_graph::AssemblyStrategy::Deep,
                other => context_graph::AssemblyStrategy::Custom(other as usize),
            };
            let enriched = guard
                .search_with_context(&req.query, limit, strategy)
                .await
                .map_err(|e| Status::internal(e.to_string()))?;
            drop(guard);
            let response = SearchResponse {
                results: enriched
                    .into_iter()
                    .map(|er| -> Result<SearchResult, Status> {
                        Ok(SearchResult {
                            file: er.primary.chunk.file_path,
                            score: f64::from(er.primary.score),
                            start_line: i32::try_from(er.primary.chunk.start_line)
                                .map_err(|e| Status::internal(e.to_string()))?,
                            end_line: i32::try_from(er.primary.chunk.end_line)
                                .map_err(|e| Status::internal(e.to_string()))?,
                            related: map_related(&er.related)?,
                        })
                    })
                    .collect::<Result<Vec<_>, Status>>()?,
            };
            return Ok(Response::new(response));
        }

        let results = guard
            .hybrid_mut()
            .search(&req.query, limit)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        drop(guard);

        let response = SearchResponse {
            results: results
                .into_iter()
                .map(|r| -> Result<SearchResult, Status> {
                    Ok(SearchResult {
                        file: r.chunk.file_path,
                        score: f64::from(r.score),
                        start_line: i32::try_from(r.chunk.start_line)
                            .map_err(|e| Status::internal(e.to_string()))?,
                        end_line: i32::try_from(r.chunk.end_line)
                            .map_err(|e| Status::internal(e.to_string()))?,
                        related: vec![],
                    })
                })
                .collect::<Result<Vec<_>, Status>>()?,
        };

        Ok(Response::new(response))
    }

    async fn get_health(
        &self,
        _request: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        let response = if let Some(streamer) = &self.streaming_indexer {
            to_health_response(Some(streamer.health_snapshot()))
        } else {
            to_health_response(None)
        };

        Ok(Response::new(response))
    }

    async fn trigger_index(
        &self,
        request: Request<TriggerIndexRequest>,
    ) -> Result<Response<TriggerIndexResponse>, Status> {
        let Some(streamer) = &self.streaming_indexer else {
            return Err(Status::failed_precondition(
                "watcher disabled for this daemon",
            ));
        };

        let payload = request.into_inner();
        let reason = if payload.reason.trim().is_empty() {
            "manual"
        } else {
            payload.reason.trim()
        };

        streamer
            .trigger(reason)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let response = TriggerIndexResponse {
            accepted: true,
            message: format!("scheduled incremental index: {reason}"),
        };
        Ok(Response::new(response))
    }
}

async fn build_context_search(config: &DaemonConfig) -> Result<ContextSearch> {
    let store_path = context_dir_for_project_root(&config.project_root).join("index.json");
    let store_mtime = tokio::fs::metadata(&store_path)
        .await
        .context("Failed to stat vector store")?
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let store = VectorStore::load(&store_path).await?;
    let (chunks, chunk_lookup) = crate::command::services::collect_chunks(&store);

    let hybrid = HybridSearch::new(store, chunks)?;
    let mut search = ContextSearch::new(hybrid)?;

    if config.context_depth > 0 {
        if let Some(lang) = config.graph_language {
            let cache_hit = match config
                .graph_cache
                .load(store_mtime, lang, &chunks, &chunk_lookup)
                .await?
            {
                Some(assembler) => {
                    log::info!("Loaded code graph from cache");
                    search.set_assembler(assembler);
                    true
                }
                None => false,
            };

            if !cache_hit {
                if let Err(err) = search.build_graph(lang) {
                    log::warn!("Failed to build code graph: {err}");
                } else if let Some(assembler) = search.assembler() {
                    if let Err(err) = config.graph_cache.save(store_mtime, lang, assembler).await {
                        log::warn!("Failed to persist graph cache: {err}");
                    }
                }
            }
        } else {
            log::warn!(
                "Context depth > 0 but no graph language specified; graph features disabled"
            );
        }
    }

    Ok(search)
}

async fn reload_search(shared: &Arc<Mutex<ContextSearch>>, config: &DaemonConfig) -> Result<()> {
    let updated = build_context_search(config).await?;
    let mut guard = shared.lock().await;
    *guard = updated;
    Ok(())
}

async fn health_log_loop(streamer: StreamingIndexer, path: PathBuf) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;

    append_health_entry(&mut file, &streamer.health_snapshot()).await?;
    let mut rx = streamer.health_stream();
    loop {
        if rx.changed().await.is_err() {
            break;
        }
        let snapshot = rx.borrow().clone();
        append_health_entry(&mut file, &snapshot).await?;
    }
    Ok(())
}

async fn append_health_entry(file: &mut tokio::fs::File, snapshot: &IndexerHealth) -> Result<()> {
    let entry = json!({
        "timestamp_unix_ms": to_unix_ms(Some(SystemTime::now())),
        "health": snapshot,
    });
    file.write_all(entry.to_string().as_bytes()).await?;
    file.write_all(b"\n").await?;
    Ok(())
}

fn map_related(related: &[context_search::RelatedContext]) -> Result<Vec<RelatedChunk>, Status> {
    related
        .iter()
        .map(|rc| {
            Ok(RelatedChunk {
                file: rc.chunk.file_path.clone(),
                start_line: i32::try_from(rc.chunk.start_line)
                    .map_err(|e| Status::internal(e.to_string()))?,
                end_line: i32::try_from(rc.chunk.end_line)
                    .map_err(|e| Status::internal(e.to_string()))?,
                relationship: rc.relationship_path.clone(),
                distance: u32::try_from(rc.distance)
                    .map_err(|e| Status::internal(e.to_string()))?,
                relevance: f64::from(rc.relevance_score),
            })
        })
        .collect()
}

fn to_health_response(snapshot: Option<IndexerHealth>) -> HealthResponse {
    if let Some(health) = snapshot {
        HealthResponse {
            has_watcher: true,
            indexing: health.indexing,
            last_success_unix_ms: to_unix_ms(health.last_success),
            last_duration_ms: health.last_duration_ms.unwrap_or(0),
            consecutive_failures: health.consecutive_failures,
            pending_events: health.pending_events as u64,
            last_error: health.last_error.unwrap_or_default(),
            files_per_second: f64::from(health.last_throughput_files_per_sec.unwrap_or(0.0)),
            index_size_bytes: health.last_index_size_bytes.unwrap_or(0),
            duration_p95_ms: health.p95_duration_ms.unwrap_or(0),
            alert_log_json: health.alert_log_json,
        }
    } else {
        HealthResponse {
            has_watcher: false,
            indexing: false,
            last_success_unix_ms: 0,
            last_duration_ms: 0,
            consecutive_failures: 0,
            pending_events: 0,
            last_error: String::new(),
            files_per_second: 0.0,
            index_size_bytes: 0,
            duration_p95_ms: 0,
            alert_log_json: String::new(),
        }
    }
}

fn to_unix_ms(ts: Option<SystemTime>) -> u64 {
    ts.and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
