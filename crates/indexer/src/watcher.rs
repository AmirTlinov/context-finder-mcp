use crate::{
    health::write_health_snapshot,
    scanner::{FileScanner, IGNORED_SCOPES},
    IndexStats, IndexerError, ModelIndexSpec, MultiModelProjectIndexer, ProjectIndexer, Result,
};
use context_vector_store::{context_dir_for_project_root, current_model_id};
use ignore::WalkBuilder;
use log::{error, info, warn};
use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{broadcast, mpsc, watch, Mutex as TokioMutex};
use tokio::time;

const DEFAULT_ALERT_REASON: &str = "fs_event";
const REFRESH_MODELS_REASON_PREFIX: &str = "refresh_models:";

fn parse_refresh_models_reason(reason: &str) -> Option<Vec<String>> {
    let tail = reason.strip_prefix(REFRESH_MODELS_REASON_PREFIX)?;
    let (csv, _) = tail.split_once(':').unwrap_or((tail, ""));
    let mut ids = Vec::new();
    for raw in csv.split(',') {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        ids.push(trimmed.to_string());
    }
    (!ids.is_empty()).then_some(ids)
}

#[derive(Debug, Clone)]
pub struct IndexUpdate {
    pub completed_at: SystemTime,
    pub duration_ms: u64,
    pub stats: Option<IndexStats>,
    pub success: bool,
    pub reason: String,
    pub store_size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexerHealth {
    pub last_success: Option<SystemTime>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
    pub last_duration_ms: Option<u64>,
    pub pending_events: usize,
    pub indexing: bool,
    pub last_throughput_files_per_sec: Option<f32>,
    pub p95_duration_ms: Option<u64>,
    pub last_index_size_bytes: Option<u64>,
    pub alert_log_json: String,
    pub alert_log_len: usize,
}

impl IndexerHealth {
    fn initial() -> Self {
        Self {
            last_success: None,
            last_error: None,
            consecutive_failures: 0,
            last_duration_ms: None,
            pending_events: 0,
            indexing: false,
            last_throughput_files_per_sec: None,
            p95_duration_ms: None,
            last_index_size_bytes: None,
            alert_log_json: String::from("[]"),
            alert_log_len: 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StreamingIndexerConfig {
    pub debounce: Duration,
    pub max_batch_wait: Duration,
    pub notify_poll_interval: Duration,
}

impl Default for StreamingIndexerConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(750),
            max_batch_wait: Duration::from_secs(3),
            notify_poll_interval: Duration::from_secs(2),
        }
    }
}

#[derive(Clone)]
pub struct StreamingIndexer {
    inner: Arc<StreamingIndexerInner>,
}

struct StreamingIndexerInner {
    command_tx: mpsc::Sender<WatcherCommand>,
    update_tx: broadcast::Sender<IndexUpdate>,
    health_tx: watch::Sender<IndexerHealth>,
    _watcher: Arc<std::sync::Mutex<Option<RecommendedWatcher>>>,
    _watch_state: Arc<std::sync::Mutex<HashSet<PathBuf>>>,
    _health_guard: TokioMutex<watch::Receiver<IndexerHealth>>,
}

enum WatcherCommand {
    Trigger { reason: String },
    Shutdown,
}

impl StreamingIndexer {
    pub fn start(indexer: Arc<ProjectIndexer>, config: StreamingIndexerConfig) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel(1024);
        let (command_tx, command_rx) = mpsc::channel(16);
        let (health_tx, health_rx) = watch::channel(IndexerHealth::initial());
        let (update_tx, _) = broadcast::channel(32);

        let (watcher, watch_state) =
            create_fs_watcher(indexer.root(), event_tx, config.notify_poll_interval)?;
        let watcher = Arc::new(std::sync::Mutex::new(Some(watcher)));

        spawn_index_loop(
            indexer,
            config,
            event_rx,
            command_rx,
            update_tx.clone(),
            health_tx.clone(),
            watcher.clone(),
            watch_state.clone(),
        );

        Ok(Self {
            inner: Arc::new(StreamingIndexerInner {
                command_tx,
                update_tx,
                health_tx,
                _watcher: watcher,
                _watch_state: watch_state,
                _health_guard: TokioMutex::new(health_rx),
            }),
        })
    }

    pub async fn trigger(&self, reason: impl Into<String>) -> Result<()> {
        self.inner
            .command_tx
            .send(WatcherCommand::Trigger {
                reason: reason.into(),
            })
            .await
            .map_err(|e| IndexerError::Other(format!("failed to send trigger: {e}")))?;
        Ok(())
    }

    #[must_use]
    pub fn subscribe_updates(&self) -> broadcast::Receiver<IndexUpdate> {
        self.inner.update_tx.subscribe()
    }

    #[must_use]
    pub fn health_snapshot(&self) -> IndexerHealth {
        self.inner.health_tx.subscribe().borrow().clone()
    }

    #[must_use]
    pub fn health_stream(&self) -> watch::Receiver<IndexerHealth> {
        self.inner.health_tx.subscribe()
    }
}

impl Drop for StreamingIndexer {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            let _ = self.inner.command_tx.try_send(WatcherCommand::Shutdown);
        }
    }
}

#[derive(Clone)]
pub struct MultiModelStreamingIndexer {
    inner: Arc<MultiModelStreamingIndexerInner>,
}

struct MultiModelStreamingIndexerInner {
    command_tx: mpsc::Sender<WatcherCommand>,
    update_tx: broadcast::Sender<IndexUpdate>,
    health_tx: watch::Sender<IndexerHealth>,
    _watcher: Arc<std::sync::Mutex<Option<RecommendedWatcher>>>,
    _watch_state: Arc<std::sync::Mutex<HashSet<PathBuf>>>,
    _health_guard: TokioMutex<watch::Receiver<IndexerHealth>>,
    models: Arc<TokioMutex<Vec<ModelIndexSpec>>>,
}

impl MultiModelStreamingIndexer {
    pub fn start(
        indexer: Arc<MultiModelProjectIndexer>,
        models: Vec<ModelIndexSpec>,
        config: StreamingIndexerConfig,
    ) -> Result<Self> {
        if models.is_empty() {
            return Err(IndexerError::Other(
                "MultiModelStreamingIndexer requires at least one model".to_string(),
            ));
        }

        let (event_tx, event_rx) = mpsc::channel(1024);
        let (command_tx, command_rx) = mpsc::channel(16);
        let (health_tx, health_rx) = watch::channel(IndexerHealth::initial());
        let (update_tx, _) = broadcast::channel(32);

        let (watcher, watch_state) =
            create_fs_watcher(indexer.root(), event_tx, config.notify_poll_interval)?;
        let watcher = Arc::new(std::sync::Mutex::new(Some(watcher)));

        let models = Arc::new(TokioMutex::new(models));

        spawn_multi_model_index_loop(
            indexer,
            config,
            event_rx,
            command_rx,
            update_tx.clone(),
            health_tx.clone(),
            models.clone(),
            watcher.clone(),
            watch_state.clone(),
        );

        Ok(Self {
            inner: Arc::new(MultiModelStreamingIndexerInner {
                command_tx,
                update_tx,
                health_tx,
                _watcher: watcher,
                _watch_state: watch_state,
                _health_guard: TokioMutex::new(health_rx),
                models,
            }),
        })
    }

    pub async fn trigger(&self, reason: impl Into<String>) -> Result<()> {
        self.inner
            .command_tx
            .send(WatcherCommand::Trigger {
                reason: reason.into(),
            })
            .await
            .map_err(|e| IndexerError::Other(format!("failed to send trigger: {e}")))?;
        Ok(())
    }

    pub async fn set_models(&self, models: Vec<ModelIndexSpec>) -> Result<()> {
        if models.is_empty() {
            return Err(IndexerError::Other(
                "MultiModelStreamingIndexer models must not be empty".to_string(),
            ));
        }
        {
            let mut guard = self.inner.models.lock().await;
            *guard = models;
        }
        Ok(())
    }

    #[must_use]
    pub fn subscribe_updates(&self) -> broadcast::Receiver<IndexUpdate> {
        self.inner.update_tx.subscribe()
    }

    #[must_use]
    pub fn health_snapshot(&self) -> IndexerHealth {
        self.inner.health_tx.subscribe().borrow().clone()
    }

    #[must_use]
    pub fn health_stream(&self) -> watch::Receiver<IndexerHealth> {
        self.inner.health_tx.subscribe()
    }
}

impl Drop for MultiModelStreamingIndexer {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            let _ = self.inner.command_tx.try_send(WatcherCommand::Shutdown);
        }
    }
}

fn create_fs_watcher(
    root: &Path,
    sender: mpsc::Sender<notify::Result<Event>>,
    poll_interval: Duration,
) -> Result<(RecommendedWatcher, Arc<std::sync::Mutex<HashSet<PathBuf>>>)> {
    let root = root.to_path_buf();
    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = sender.blocking_send(res);
        },
        NotifyConfig::default().with_poll_interval(poll_interval),
    )
    .map_err(|e| IndexerError::Other(format!("watcher init failed: {e}")))?;
    let watch_state = Arc::new(std::sync::Mutex::new(HashSet::new()));
    let watch_dirs = build_watch_list(&root);
    {
        let mut guard = watch_state
            .lock()
            .map_err(|_| IndexerError::Other("watch state lock poisoned".to_string()))?;
        for dir in watch_dirs {
            if let Err(err) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                warn!("failed to watch {}: {err}", dir.display());
                continue;
            }
            guard.insert(dir);
        }
    }
    Ok((watcher, watch_state))
}

fn build_watch_list(root: &Path) -> Vec<PathBuf> {
    let mut out: HashSet<PathBuf> = HashSet::new();
    out.insert(root.to_path_buf());

    let root_owned = root.to_path_buf();
    let mut builder = WalkBuilder::new(&root_owned);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);
    builder.filter_entry(move |entry| is_watchable_dir(&root_owned, entry.path()));

    for entry in builder.build() {
        let Ok(entry) = entry else {
            continue;
        };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        if is_watchable_dir(root, path) {
            out.insert(path.to_path_buf());
        }
    }

    out.into_iter().collect()
}

fn maybe_add_watches(
    root: &Path,
    evt: &Event,
    watcher: &Arc<std::sync::Mutex<Option<RecommendedWatcher>>>,
    watch_state: &Arc<std::sync::Mutex<HashSet<PathBuf>>>,
) {
    if evt.paths.is_empty() {
        return;
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    for path in &evt.paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        if !is_watchable_dir(root, path) {
            continue;
        }
        candidates.push(path.to_path_buf());
    }

    for path in candidates {
        add_watch_tree(root, &path, watcher, watch_state);
    }
}

fn add_watch_tree(
    root: &Path,
    start: &Path,
    watcher: &Arc<std::sync::Mutex<Option<RecommendedWatcher>>>,
    watch_state: &Arc<std::sync::Mutex<HashSet<PathBuf>>>,
) {
    let mut builder = WalkBuilder::new(start);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);
    let root_owned = root.to_path_buf();
    builder.filter_entry(move |entry| is_watchable_dir(&root_owned, entry.path()));

    let mut to_add: Vec<PathBuf> = Vec::new();
    for entry in builder.build() {
        let Ok(entry) = entry else {
            continue;
        };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        if is_watchable_dir(root, path) {
            to_add.push(path.to_path_buf());
        }
    }

    if to_add.is_empty() {
        return;
    }

    let mut new_dirs: Vec<PathBuf> = Vec::new();
    {
        let mut guard = match watch_state.lock() {
            Ok(guard) => guard,
            Err(_) => {
                warn!("watch state lock poisoned");
                return;
            }
        };
        for dir in to_add {
            if guard.insert(dir.clone()) {
                new_dirs.push(dir);
            }
        }
    }

    if new_dirs.is_empty() {
        return;
    }

    let mut watcher_guard = match watcher.lock() {
        Ok(guard) => guard,
        Err(_) => {
            warn!("watcher lock poisoned");
            return;
        }
    };
    let Some(watcher) = watcher_guard.as_mut() else {
        return;
    };
    for dir in new_dirs {
        if let Err(err) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
            warn!("failed to watch {}: {err}", dir.display());
            if let Ok(mut guard) = watch_state.lock() {
                guard.remove(&dir);
            }
        }
    }
}

fn is_watchable_dir(root: &Path, path: &Path) -> bool {
    if path == root {
        return true;
    }
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };

    for component in relative.components() {
        if let std::path::Component::Normal(name) = component {
            let lowered = name.to_string_lossy().to_lowercase();
            if IGNORED_SCOPES.iter().any(|ignored| ignored == &lowered) {
                return false;
            }
            if lowered.starts_with('.') && !FileScanner::is_allowlisted_hidden(&lowered) {
                return false;
            }
        }
    }
    true
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn spawn_index_loop(
    indexer: Arc<ProjectIndexer>,
    config: StreamingIndexerConfig,
    mut event_rx: mpsc::Receiver<notify::Result<Event>>,
    mut command_rx: mpsc::Receiver<WatcherCommand>,
    update_tx: broadcast::Sender<IndexUpdate>,
    health_tx: watch::Sender<IndexerHealth>,
    watcher: Arc<std::sync::Mutex<Option<RecommendedWatcher>>>,
    watch_state: Arc<std::sync::Mutex<HashSet<PathBuf>>>,
) {
    tokio::spawn(async move {
        let mut state = DebounceState::new(config.debounce, config.max_batch_wait);
        let mut health = IndexerHealth::initial();
        let mut duration_history: VecDeque<u64> = VecDeque::new();
        let mut alert_log: VecDeque<AlertRecord> = VecDeque::new();

        loop {
            let next_deadline = state.next_deadline();

            tokio::select! {
                Some(event) = event_rx.recv() => {
                    if handle_event(indexer.root(), event, &mut state, &watcher, &watch_state) {
                        health.pending_events = state.pending();
                        let _ = health_tx.send(health.clone());
                    }
                }
                Some(cmd) = command_rx.recv() => {
                    match cmd {
                        WatcherCommand::Trigger { reason } => {
                            state.force_run(reason);
                            health.pending_events = state.pending();
                            let _ = health_tx.send(health.clone());
                        }
                        WatcherCommand::Shutdown => break,
                    }
                }
                () = async {
                    if let Some(deadline) = next_deadline {
                        time::sleep_until(deadline).await;
                    }
                }, if state.should_run() && next_deadline.is_some() => {
                    health.indexing = true;
                    let _ = health_tx.send(health.clone());

                    let pending_before_run = state.pending();
                    let paths_hint = state.take_paths_hint();
                    match run_index_cycle(
                        indexer.clone(),
                        state
                            .take_reason()
                            .unwrap_or_else(|| DEFAULT_ALERT_REASON.to_string()),
                        paths_hint,
                    )
                    .await {
                        Ok((cycle_stats, duration, reason, store_size)) => {
                            health.last_success = Some(SystemTime::now());
                            health.last_duration_ms = Some(duration);
                            health.last_error = None;
                            health.consecutive_failures = 0;
                            health.indexing = false;
                            health.pending_events = 0;
                            if duration > 0 {
                                #[allow(clippy::cast_precision_loss)]
                                let files_per_sec =
                                    cycle_stats.files as f32 / (duration as f32 / 1000.0);
                                health.last_throughput_files_per_sec = Some(files_per_sec);
                            }
                            health.last_index_size_bytes = store_size;
                            record_duration(&mut duration_history, duration);
                            health.p95_duration_ms = compute_p95(&duration_history);
                            health.alert_log_json = serialize_alerts(&alert_log);
                            health.alert_log_len = alert_log.len();
                            state.tune_after_cycle(duration, health.p95_duration_ms, pending_before_run, true);
                            if let Err(err) = write_health_snapshot(
                                indexer.root(),
                                &cycle_stats,
                                &reason,
                                health.p95_duration_ms,
                                Some(health.pending_events),
                            )
                            .await
                            {
                                warn!("Failed to persist health snapshot after watcher index: {err}");
                            }
                            let _ = health_tx.send(health.clone());
                            let _ = update_tx.send(IndexUpdate {
                                completed_at: SystemTime::now(),
                                duration_ms: duration,
                                stats: Some(cycle_stats.clone()),
                                success: true,
                                reason,
                                store_size_bytes: store_size,
                            });
                        }
                        Err((err, duration, reason)) => {
                            error!("Streaming index failure: {err}");
                            health.last_error = Some(err.clone());
                            health.consecutive_failures += 1;
                            health.last_duration_ms = Some(duration);
                            health.indexing = false;
                            health.pending_events = 0;
                            state.tune_after_cycle(duration, health.p95_duration_ms, pending_before_run, false);
                            if let Err(e) = crate::append_failure_reason(
                                indexer.root(),
                                &reason,
                                &err,
                                health.p95_duration_ms,
                            )
                            .await
                            {
                                warn!("Failed to persist failure reason: {e}");
                            }
                            push_alert(&mut alert_log, "error", &reason, &err);
                            health.alert_log_json = serialize_alerts(&alert_log);
                            health.alert_log_len = alert_log.len();
                            let _ = health_tx.send(health.clone());
                            let _ = update_tx.send(IndexUpdate {
                                completed_at: SystemTime::now(),
                                duration_ms: duration,
                                stats: None,
                                success: false,
                                reason,
                                store_size_bytes: None,
                            });
                        }
                    }

                    state.reset();
                }
            }
        }
    });
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn spawn_multi_model_index_loop(
    indexer: Arc<MultiModelProjectIndexer>,
    config: StreamingIndexerConfig,
    mut event_rx: mpsc::Receiver<notify::Result<Event>>,
    mut command_rx: mpsc::Receiver<WatcherCommand>,
    update_tx: broadcast::Sender<IndexUpdate>,
    health_tx: watch::Sender<IndexerHealth>,
    models: Arc<TokioMutex<Vec<ModelIndexSpec>>>,
    watcher: Arc<std::sync::Mutex<Option<RecommendedWatcher>>>,
    watch_state: Arc<std::sync::Mutex<HashSet<PathBuf>>>,
) {
    tokio::spawn(async move {
        let mut state = DebounceState::new(config.debounce, config.max_batch_wait);
        let mut health = IndexerHealth::initial();
        let mut duration_history: VecDeque<u64> = VecDeque::new();
        let mut alert_log: VecDeque<AlertRecord> = VecDeque::new();

        loop {
            let next_deadline = state.next_deadline();

            tokio::select! {
                Some(event) = event_rx.recv() => {
                    if handle_event(indexer.root(), event, &mut state, &watcher, &watch_state) {
                        health.pending_events = state.pending();
                        let _ = health_tx.send(health.clone());
                    }
                }
                Some(cmd) = command_rx.recv() => {
                    match cmd {
                        WatcherCommand::Trigger { reason } => {
                            state.force_run(reason);
                            health.pending_events = state.pending();
                            let _ = health_tx.send(health.clone());
                        }
                        WatcherCommand::Shutdown => break,
                    }
                }
                () = async {
                    if let Some(deadline) = next_deadline {
                        time::sleep_until(deadline).await;
                    }
                }, if state.should_run() && next_deadline.is_some() => {
                    health.indexing = true;
                    let _ = health_tx.send(health.clone());

                    let snapshot_models = {
                        let guard = models.lock().await;
                        guard.clone()
                    };

                    if snapshot_models.is_empty() {
                        warn!("Multi-model watcher has no configured models; skipping index cycle");
                        health.indexing = false;
                        health.pending_events = 0;
                        let _ = health_tx.send(health.clone());
                        state.reset();
                        continue;
                    }

                    let pending_before_run = state.pending();
                    let paths_hint = state.take_paths_hint();
                    match run_multi_model_index_cycle(
                        indexer.clone(),
                        snapshot_models,
                        state
                            .take_reason()
                            .unwrap_or_else(|| DEFAULT_ALERT_REASON.to_string()),
                        paths_hint,
                    )
                    .await {
                        Ok((cycle_stats, duration, reason, store_size)) => {
                            health.last_success = Some(SystemTime::now());
                            health.last_duration_ms = Some(duration);
                            health.last_error = None;
                            health.consecutive_failures = 0;
                            health.indexing = false;
                            health.pending_events = 0;
                            if duration > 0 {
                                #[allow(clippy::cast_precision_loss)]
                                let files_per_sec =
                                    cycle_stats.files as f32 / (duration as f32 / 1000.0);
                                health.last_throughput_files_per_sec = Some(files_per_sec);
                            }
                            health.last_index_size_bytes = store_size;
                            record_duration(&mut duration_history, duration);
                            health.p95_duration_ms = compute_p95(&duration_history);
                            health.alert_log_json = serialize_alerts(&alert_log);
                            health.alert_log_len = alert_log.len();
                            state.tune_after_cycle(duration, health.p95_duration_ms, pending_before_run, true);
                            if let Err(err) = write_health_snapshot(
                                indexer.root(),
                                &cycle_stats,
                                &reason,
                                health.p95_duration_ms,
                                Some(health.pending_events),
                            )
                            .await
                            {
                                warn!("Failed to persist health snapshot after watcher index: {err}");
                            }
                            let _ = health_tx.send(health.clone());
                            let _ = update_tx.send(IndexUpdate {
                                completed_at: SystemTime::now(),
                                duration_ms: duration,
                                stats: Some(cycle_stats.clone()),
                                success: true,
                                reason,
                                store_size_bytes: store_size,
                            });
                        }
                        Err((err, duration, reason)) => {
                            error!("Streaming index failure: {err}");
                            health.last_error = Some(err.clone());
                            health.consecutive_failures += 1;
                            health.last_duration_ms = Some(duration);
                            health.indexing = false;
                            health.pending_events = 0;
                            state.tune_after_cycle(duration, health.p95_duration_ms, pending_before_run, false);
                            if let Err(e) = crate::append_failure_reason(
                                indexer.root(),
                                &reason,
                                &err,
                                health.p95_duration_ms,
                            )
                            .await
                            {
                                warn!("Failed to persist failure reason: {e}");
                            }
                            push_alert(&mut alert_log, "error", &reason, &err);
                            health.alert_log_json = serialize_alerts(&alert_log);
                            health.alert_log_len = alert_log.len();
                            let _ = health_tx.send(health.clone());
                            let _ = update_tx.send(IndexUpdate {
                                completed_at: SystemTime::now(),
                                duration_ms: duration,
                                stats: None,
                                success: false,
                                reason,
                                store_size_bytes: None,
                            });
                        }
                    }

                    state.reset();
                }
            }
        }
    });
}

async fn run_index_cycle(
    indexer: Arc<ProjectIndexer>,
    reason: String,
    paths_hint: Option<Vec<PathBuf>>,
) -> std::result::Result<(IndexStats, u64, String, Option<u64>), (String, u64, String)> {
    let started = Instant::now();
    let outcome = match paths_hint {
        Some(paths) => indexer.index_changed_paths(&paths).await,
        None => indexer.index().await,
    };
    match outcome {
        Ok(stats) => {
            #[allow(clippy::cast_possible_truncation)]
            let duration = started.elapsed().as_millis() as u64;
            info!("Incremental index finished in {duration}ms");
            let store_size = tokio::fs::metadata(indexer.store_path())
                .await
                .ok()
                .map(|meta| meta.len());
            Ok((stats, duration, reason, store_size))
        }
        Err(e) => {
            #[allow(clippy::cast_possible_truncation)]
            let duration = started.elapsed().as_millis() as u64;
            Err((e.to_string(), duration, reason))
        }
    }
}

async fn run_multi_model_index_cycle(
    indexer: Arc<MultiModelProjectIndexer>,
    models: Vec<ModelIndexSpec>,
    reason: String,
    paths_hint: Option<Vec<PathBuf>>,
) -> std::result::Result<(IndexStats, u64, String, Option<u64>), (String, u64, String)> {
    let started = Instant::now();
    let all_models = models;
    let mut active_models: Vec<ModelIndexSpec> = Vec::new();
    let refresh_models = parse_refresh_models_reason(&reason);
    let active_models_slice: &[ModelIndexSpec] = if let Some(refresh_models) = refresh_models {
        let refresh_set: HashSet<String> = refresh_models.into_iter().collect();
        for spec in &all_models {
            if refresh_set.contains(&spec.model_id) {
                active_models.push(spec.clone());
            }
        }
        if active_models.is_empty() {
            &all_models
        } else {
            &active_models
        }
    } else if reason == DEFAULT_ALERT_REASON && all_models.len() > 1 {
        let primary_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
        if let Some(primary) = all_models
            .iter()
            .find(|spec| spec.model_id == primary_id)
            .cloned()
            .or_else(|| all_models.first().cloned())
        {
            active_models.push(primary);
        }
        &active_models
    } else {
        &all_models
    };

    let outcome = match paths_hint {
        Some(paths) => {
            indexer
                .index_models_changed_paths(active_models_slice, &paths)
                .await
        }
        None => indexer.index_models(active_models_slice, false).await,
    };
    match outcome {
        Ok(stats) => {
            #[allow(clippy::cast_possible_truncation)]
            let duration = started.elapsed().as_millis() as u64;
            info!("Incremental multi-model index finished in {duration}ms");
            let store_size = sum_model_store_sizes(indexer.root(), &all_models).await;
            Ok((stats, duration, reason, store_size))
        }
        Err(e) => {
            #[allow(clippy::cast_possible_truncation)]
            let duration = started.elapsed().as_millis() as u64;
            Err((e.to_string(), duration, reason))
        }
    }
}

async fn sum_model_store_sizes(root: &Path, models: &[ModelIndexSpec]) -> Option<u64> {
    let mut sum = 0u64;
    let mut any = false;
    for spec in models {
        let path = context_dir_for_project_root(root)
            .join("indexes")
            .join(model_id_dir_name(&spec.model_id))
            .join("index.json");
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            sum = sum.saturating_add(meta.len());
            any = true;
        }
    }
    any.then_some(sum)
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

fn handle_event(
    root: &Path,
    event: notify::Result<Event>,
    state: &mut DebounceState,
    watcher: &Arc<std::sync::Mutex<Option<RecommendedWatcher>>>,
    watch_state: &Arc<std::sync::Mutex<HashSet<PathBuf>>>,
) -> bool {
    match event {
        Ok(evt) => {
            maybe_add_watches(root, &evt, watcher, watch_state);
            if evt.paths.is_empty() {
                state.record_event(1, DEFAULT_ALERT_REASON);
                return true;
            }

            let mut relevant_new = 0usize;
            let mut saw_relevant = false;
            for path in evt.paths {
                if is_relevant_path(root, &path) {
                    saw_relevant = true;
                    if state.record_path_if_new(&path) {
                        relevant_new = relevant_new.saturating_add(1);
                    }
                }
            }
            if saw_relevant {
                state.record_event(relevant_new, DEFAULT_ALERT_REASON);
                return true;
            }
            false
        }
        Err(err) => {
            warn!("Watcher error: {err}");
            false
        }
    }
}

fn is_relevant_path(root: &Path, path: &Path) -> bool {
    if let Ok(relative) = path.strip_prefix(root) {
        for component in relative.components() {
            if let std::path::Component::Normal(name) = component {
                let name = name.to_string_lossy();
                if IGNORED_SCOPES
                    .iter()
                    .any(|ignored| name.eq_ignore_ascii_case(ignored))
                {
                    return false;
                }

                if name.starts_with('.') {
                    let lowered = name.to_lowercase();
                    if lowered != ".gitignore" && !FileScanner::is_allowlisted_hidden(&lowered) {
                        return false;
                    }
                }
            }
        }

        if is_bench_logs_json(path) {
            return false;
        }

        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let lowered = name.to_lowercase();
            if lowered != ".gitignore" {
                if FileScanner::is_noise_file(path) {
                    return false;
                }
                if FileScanner::is_secret_file(path) {
                    return false;
                }
            }
        }
    }

    true
}

fn is_bench_logs_json(path: &Path) -> bool {
    let is_json = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
    if !is_json {
        return false;
    }

    let Some(parent) = path.parent() else {
        return false;
    };
    if !path_component_matches(parent, "logs") {
        return false;
    }

    parent
        .parent()
        .is_some_and(|grand| path_component_matches(grand, "bench"))
}

fn path_component_matches(path: &Path, target: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(target))
}

#[derive(Debug, Serialize)]
struct AlertRecord {
    timestamp_unix_ms: u64,
    level: String,
    reason: String,
    detail: String,
}

struct DebounceState {
    debounce: Duration,
    max_batch: Duration,
    dirty: bool,
    pending: usize,
    last_event: Option<Instant>,
    first_event: Option<Instant>,
    reason: Option<String>,
    force_immediate: bool,
    force_full_scan: bool,
    recent_paths: VecDeque<(String, Instant)>,
    pending_paths: HashSet<PathBuf>,
    dedup_window: Duration,
    tune_stable_cycles: u8,
}

impl DebounceState {
    fn new(debounce: Duration, max_batch: Duration) -> Self {
        Self {
            debounce,
            max_batch,
            dirty: false,
            pending: 0,
            last_event: None,
            first_event: None,
            reason: None,
            force_immediate: false,
            force_full_scan: false,
            recent_paths: VecDeque::new(),
            pending_paths: HashSet::new(),
            dedup_window: Duration::from_millis(750),
            tune_stable_cycles: 0,
        }
    }

    fn record_event(&mut self, count: usize, reason: &str) {
        self.pending += count.max(1);
        self.reason = Some(reason.to_string());
        self.last_event = Some(Instant::now());
        self.first_event.get_or_insert_with(Instant::now);
        self.dirty = true;
    }

    fn force_run(&mut self, reason: String) {
        self.pending += 1;
        self.reason = Some(reason);
        self.force_immediate = true;
        self.dirty = true;
    }

    const fn pending(&self) -> usize {
        self.pending
    }

    const fn should_run(&self) -> bool {
        self.dirty
    }

    fn next_deadline(&self) -> Option<time::Instant> {
        if !self.dirty {
            return None;
        }

        if self.force_immediate {
            return Some(time::Instant::now());
        }

        let mut deadline = self.last_event.map(|last| last + self.debounce);

        if let Some(first) = self.first_event {
            let forced = first + self.max_batch;
            deadline = Some(match deadline {
                Some(current) if forced < current => forced,
                Some(current) => current,
                None => forced,
            });
        }

        deadline.map(time::Instant::from_std)
    }

    #[allow(clippy::missing_const_for_fn)]
    fn take_reason(&mut self) -> Option<String> {
        self.reason.take()
    }

    fn take_paths_hint(&mut self) -> Option<Vec<PathBuf>> {
        if self.force_full_scan {
            return None;
        }
        if self.pending_paths.is_empty() {
            return None;
        }
        Some(self.pending_paths.drain().collect())
    }

    fn reset(&mut self) {
        self.dirty = false;
        self.pending = 0;
        self.last_event = None;
        self.first_event = None;
        self.reason = None;
        self.force_immediate = false;
        self.force_full_scan = false;
        self.recent_paths.clear();
        self.pending_paths.clear();
    }

    #[cfg(test)]
    const fn force_flag(&self) -> bool {
        self.force_immediate
    }

    fn record_path_if_new(&mut self, path: &Path) -> bool {
        if self.force_full_scan {
            return true;
        }

        let now = Instant::now();
        let key = path.to_string_lossy().to_string();
        self.recent_paths
            .retain(|(_, ts)| now.duration_since(*ts) <= self.dedup_window);
        let already = self.recent_paths.iter().any(|(p, _)| p == &key);
        if already {
            false
        } else {
            self.recent_paths.push_back((key, now));
            const MAX_DELTA_PATHS: usize = 512;
            if self.pending_paths.len() >= MAX_DELTA_PATHS {
                self.force_full_scan = true;
                self.pending_paths.clear();
            } else {
                self.pending_paths.insert(path.to_path_buf());
            }
            true
        }
    }

    fn tune_after_cycle(
        &mut self,
        duration_ms: u64,
        p95_duration_ms: Option<u64>,
        pending_events: usize,
        success: bool,
    ) {
        const DEBOUNCE_LEVELS_MS: &[u64] = &[500, 750, 1_000, 2_000, 3_000, 4_000, 5_000];
        const MAX_BATCH_LEVELS_MS: &[u64] = &[3_000, 5_000, 10_000, 20_000, 30_000];

        fn duration_to_ms(d: Duration) -> u64 {
            u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
        }

        fn snap_up(ms: u64, levels: &[u64]) -> u64 {
            for &level in levels {
                if ms <= level {
                    return level;
                }
            }
            *levels.last().expect("levels non-empty")
        }

        fn idx_of(level_ms: u64, levels: &[u64]) -> usize {
            levels
                .iter()
                .position(|&v| v == level_ms)
                .unwrap_or_else(|| levels.len().saturating_sub(1))
        }

        fn step_down(current_ms: u64, target_ms: u64, levels: &[u64]) -> u64 {
            if current_ms <= target_ms {
                return current_ms;
            }
            let current_idx = idx_of(current_ms, levels);
            if current_idx == 0 {
                return current_ms;
            }
            let next = levels[current_idx - 1];
            next.max(target_ms)
        }

        let load_ms = p95_duration_ms.unwrap_or(duration_ms).max(1);

        let mut target_debounce_ms: u64 = if load_ms <= 250 {
            500
        } else if load_ms <= 500 {
            750
        } else if load_ms <= 1_000 {
            1_000
        } else if load_ms <= 2_000 {
            2_000
        } else if load_ms <= 4_000 {
            3_000
        } else {
            4_000
        };

        if pending_events >= 512 {
            target_debounce_ms = target_debounce_ms.saturating_add(2_000);
        } else if pending_events >= 256 {
            target_debounce_ms = target_debounce_ms.saturating_add(1_000);
        } else if pending_events >= 128 {
            target_debounce_ms = target_debounce_ms.saturating_add(500);
        } else if pending_events >= 64 {
            target_debounce_ms = target_debounce_ms.saturating_add(250);
        }

        if !success {
            target_debounce_ms = target_debounce_ms.max(2_000);
        }
        target_debounce_ms = target_debounce_ms.min(5_000);
        target_debounce_ms = snap_up(target_debounce_ms, DEBOUNCE_LEVELS_MS);

        let mut target_max_batch_ms = load_ms.saturating_mul(4).clamp(3_000, 30_000);
        if pending_events >= 512 {
            target_max_batch_ms = target_max_batch_ms.saturating_add(20_000);
        } else if pending_events >= 256 {
            target_max_batch_ms = target_max_batch_ms.saturating_add(10_000);
        } else if pending_events >= 128 {
            target_max_batch_ms = target_max_batch_ms.saturating_add(5_000);
        } else if pending_events >= 64 {
            target_max_batch_ms = target_max_batch_ms.saturating_add(2_000);
        }

        let min_batch_ms = target_debounce_ms.saturating_mul(5);
        target_max_batch_ms = target_max_batch_ms.max(min_batch_ms).min(30_000);
        if !success {
            target_max_batch_ms = target_max_batch_ms.max(10_000);
        }
        target_max_batch_ms = snap_up(target_max_batch_ms, MAX_BATCH_LEVELS_MS);

        let current_debounce_ms = snap_up(duration_to_ms(self.debounce), DEBOUNCE_LEVELS_MS);
        let current_max_batch_ms = snap_up(duration_to_ms(self.max_batch), MAX_BATCH_LEVELS_MS);

        let upshift =
            target_debounce_ms > current_debounce_ms || target_max_batch_ms > current_max_batch_ms;
        if upshift {
            self.debounce = Duration::from_millis(target_debounce_ms);
            self.max_batch = Duration::from_millis(target_max_batch_ms);
            self.tune_stable_cycles = 0;
            return;
        }

        let stable_churn = pending_events <= 8;
        if stable_churn {
            self.tune_stable_cycles = self.tune_stable_cycles.saturating_add(1);
        } else {
            self.tune_stable_cycles = 0;
        }

        if self.tune_stable_cycles < 3 {
            return;
        }
        self.tune_stable_cycles = 0;

        let next_debounce_ms =
            step_down(current_debounce_ms, target_debounce_ms, DEBOUNCE_LEVELS_MS);
        let next_max_batch_ms = step_down(
            current_max_batch_ms,
            target_max_batch_ms,
            MAX_BATCH_LEVELS_MS,
        );
        self.debounce = Duration::from_millis(next_debounce_ms);
        self.max_batch = Duration::from_millis(next_max_batch_ms);
    }
}

fn record_duration(history: &mut VecDeque<u64>, duration: u64) {
    const MAX_HISTORY: usize = 20;
    history.push_back(duration);
    if history.len() > MAX_HISTORY {
        history.pop_front();
    }
}

fn compute_p95(history: &VecDeque<u64>) -> Option<u64> {
    if history.is_empty() {
        return None;
    }
    let mut sorted: Vec<u64> = history.iter().copied().collect();
    sorted.sort_unstable();
    let idx = ((sorted.len().saturating_sub(1) * 95) + 50) / 100;
    sorted.get(idx).copied()
}

fn push_alert(log: &mut VecDeque<AlertRecord>, level: &str, reason: &str, detail: &str) {
    const MAX_ALERTS: usize = 20;
    let record = AlertRecord {
        timestamp_unix_ms: current_unix_ms(),
        level: level.to_string(),
        reason: reason.to_string(),
        detail: detail.to_string(),
    };
    log.push_back(record);
    if log.len() > MAX_ALERTS {
        log.pop_front();
    }
}

fn serialize_alerts(log: &VecDeque<AlertRecord>) -> String {
    serde_json::to_string(log).unwrap_or_else(|_| "[]".to_string())
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|dur| u64::try_from(dur.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::is_relevant_path;
    use super::DebounceState;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn debounce_generates_deadline() {
        let mut state = DebounceState::new(Duration::from_millis(100), Duration::from_secs(1));
        state.record_event(1, "fs_event");
        assert!(state.should_run());
        assert!(state.next_deadline().is_some());
    }

    #[test]
    fn force_run_sets_immediate_deadline() {
        let mut state = DebounceState::new(Duration::from_secs(5), Duration::from_secs(10));
        state.force_run("manual".to_string());
        assert!(state.should_run());
        assert!(state.force_flag());
        assert!(state.next_deadline().is_some());
    }

    #[test]
    fn watcher_relevance_ignores_nested_scopes() {
        let root = PathBuf::from("repo");

        let nested_node_modules = root.join("packages/web/node_modules/react/index.js");
        assert!(!is_relevant_path(&root, &nested_node_modules));

        let nested_next_cache = root.join("apps/site/.next/cache/webpack/stats.json");
        assert!(!is_relevant_path(&root, &nested_next_cache));

        let nested_bench_logs = root.join("tools/bench/logs/run.json");
        assert!(!is_relevant_path(&root, &nested_bench_logs));
    }

    #[test]
    fn watcher_relevance_ignores_non_allowlisted_hidden_scopes() {
        let root = PathBuf::from("repo");

        let pytest_cache = root.join(".pytest_cache/v/cache/lastfailed");
        assert!(!is_relevant_path(&root, &pytest_cache));
    }

    #[test]
    fn watcher_relevance_keeps_allowlisted_hidden_files() {
        let root = PathBuf::from("repo");

        let gitlab_ci = root.join(".gitlab-ci.yml");
        assert!(is_relevant_path(&root, &gitlab_ci));
    }

    #[test]
    fn watcher_relevance_treats_gitignore_as_relevant() {
        let root = PathBuf::from("repo");

        assert!(is_relevant_path(&root, &root.join(".gitignore")));
        assert!(is_relevant_path(&root, &root.join("src/.gitignore")));
    }

    #[test]
    fn watcher_relevance_ignores_known_noise_files() {
        let root = PathBuf::from("repo");

        assert!(!is_relevant_path(&root, &root.join("package-lock.json")));
        assert!(!is_relevant_path(&root, &root.join("pnpm-lock.yaml")));
        assert!(!is_relevant_path(&root, &root.join("yarn.lock")));
        assert!(!is_relevant_path(&root, &root.join("docker-compose.yml")));
        assert!(!is_relevant_path(&root, &root.join("Makefile")));
    }

    #[test]
    fn watcher_relevance_ignores_secret_files() {
        let root = PathBuf::from("repo");

        assert!(!is_relevant_path(&root, &root.join(".env")));
        assert!(!is_relevant_path(&root, &root.join(".npmrc")));
        assert!(!is_relevant_path(
            &root,
            &root.join(".cargo/credentials.toml")
        ));

        // Safe templates must stay relevant (agents often rely on them).
        assert!(is_relevant_path(&root, &root.join(".env.example")));
    }

    #[test]
    fn adaptive_tuning_upshifts_when_cycles_are_slow() {
        let mut state = DebounceState::new(Duration::from_millis(500), Duration::from_secs(3));
        state.tune_after_cycle(6_000, Some(6_000), 1, true);
        assert!(state.debounce >= Duration::from_secs(2));
        assert!(state.max_batch >= state.debounce);
    }

    #[test]
    fn adaptive_tuning_downshifts_slowly_when_quiet() {
        let mut state = DebounceState::new(Duration::from_secs(4), Duration::from_secs(20));
        for _ in 0..3 {
            state.tune_after_cycle(100, Some(100), 1, true);
        }
        assert!(state.debounce < Duration::from_secs(4));
        assert!(state.max_batch < Duration::from_secs(20));
    }

    #[test]
    fn adaptive_tuning_is_conservative_after_failure() {
        let mut state = DebounceState::new(Duration::from_millis(500), Duration::from_secs(3));
        state.tune_after_cycle(100, Some(100), 1, false);
        assert!(state.debounce >= Duration::from_secs(2));
        assert!(state.max_batch >= Duration::from_secs(10));
    }
}
