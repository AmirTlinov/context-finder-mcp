use context_indexer::{
    assess_staleness, compute_project_watermark, read_index_watermark, ModelIndexSpec,
    MultiModelProjectIndexer, MultiModelStreamingIndexer, PersistedIndexWatermark,
    StreamingIndexerConfig, Watermark,
};
use context_search::SearchProfile;
use context_vector_store::{context_dir_for_project_root, corpus_path_for_project_root, current_model_id};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;

const DEFAULT_REFRESH_DEBOUNCE: Duration = Duration::from_millis(750);
const REFRESH_MODELS_REASON_PREFIX: &str = "refresh_models:";

// Avoid stampedes right after startup when multiple agents connect at once. This is intentionally
// short: the main goal is to let interactive sessions get responsive before heavy background work.
const POLITE_UPGRADE_INITIAL_DELAY: Duration = Duration::from_secs(2);

// When the system is already busy, delay background upgrades a bit more.
const POLITE_UPGRADE_MAX_WAIT: Duration = Duration::from_secs(90);

type UpgradeTask = (
    PathBuf,
    MultiModelStreamingIndexer,
    Arc<TokioMutex<()>>,
    String,
    Vec<ModelIndexSpec>,
);

#[derive(Clone)]
struct Worker {
    streamer: MultiModelStreamingIndexer,
    models: Vec<String>,
    ttl: Duration,
    last_touch: Instant,
    last_refresh: Instant,
    refresh_debounce: Duration,
    upgrade_lock: Arc<TokioMutex<()>>,
}

impl Worker {
    fn new(
        streamer: MultiModelStreamingIndexer,
        models: Vec<String>,
        now: Instant,
        ttl: Duration,
    ) -> Self {
        Self {
            streamer,
            models,
            ttl,
            last_touch: now,
            last_refresh: now,
            refresh_debounce: DEFAULT_REFRESH_DEBOUNCE,
            upgrade_lock: Arc::new(TokioMutex::new(())),
        }
    }
}

#[derive(Default)]
struct WorkerState {
    workers: HashMap<PathBuf, Worker>,
    lru: VecDeque<PathBuf>,
    starting: HashSet<PathBuf>,
}

impl WorkerState {
    fn touch_lru(&mut self, root: &Path) {
        let key = root.to_path_buf();
        self.lru.retain(|p| p != &key);
        self.lru.push_back(key);
    }

    fn enforce_capacity(&mut self, capacity: usize) {
        let capacity = capacity.max(1);
        while self.workers.len() > capacity {
            if let Some(evict) = self.lru.pop_front() {
                self.workers.remove(&evict);
            } else {
                break;
            }
        }
    }

    fn prune_expired(&mut self, now: Instant) {
        let mut expired = Vec::new();
        for (root, worker) in &self.workers {
            if now.duration_since(worker.last_touch) >= worker.ttl {
                expired.push(root.clone());
            }
        }
        if expired.is_empty() {
            return;
        }
        for root in expired {
            self.workers.remove(&root);
            self.lru.retain(|p| p != &root);
        }
    }
}

pub struct WarmIndexers {
    cfg: StreamingIndexerConfig,
    state: WorkerState,
    worker_ttl: Duration,
    worker_capacity: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarmIndexersSnapshot {
    pub workers: usize,
    pub starting: usize,
    pub lru: usize,
}

impl WarmIndexers {
    pub fn new() -> Self {
        Self {
            cfg: StreamingIndexerConfig {
                // Agent-native default: keep background indexing low-churn during frequent edits.
                // Manual triggers (bootstrap/upgrade) bypass debounce via `force_run`.
                debounce: Duration::from_secs(2),
                max_batch_wait: Duration::from_secs(10),
                ..Default::default()
            },
            state: WorkerState::default(),
            worker_ttl: worker_ttl_from_env(),
            worker_capacity: worker_capacity_from_env(),
        }
    }

    pub fn snapshot(&self) -> WarmIndexersSnapshot {
        WarmIndexersSnapshot {
            workers: self.state.workers.len(),
            starting: self.state.starting.len(),
            lru: self.state.lru.len(),
        }
    }

    pub async fn touch(&mut self, root: &Path, profile: &SearchProfile, model_ids: Vec<String>) {
        let now = Instant::now();
        self.state.prune_expired(now);

        let templates = profile.embedding().clone();
        let (primary_spec, desired_full_specs, desired_model_ids) =
            model_specs(model_ids, templates.clone());

        if self.state.workers.contains_key(root) {
            let mut update_streamer: Option<(MultiModelStreamingIndexer, Vec<ModelIndexSpec>)> =
                None;
            let mut upgrade_task: Option<UpgradeTask> = None;
            {
                let worker = self
                    .state
                    .workers
                    .get_mut(root)
                    .expect("workers.contains_key checked");
                worker.ttl = self.worker_ttl;
                worker.last_touch = now;

                let mut merged = worker.models.clone();
                for id in &desired_model_ids {
                    if merged.iter().any(|existing| existing == id) {
                        continue;
                    }
                    merged.push(id.clone());
                }
                merged.sort();
                merged.dedup();

                if merged != worker.models {
                    worker.models = merged;
                    let (primary_spec, full_specs, _) =
                        model_specs(worker.models.clone(), templates.clone());
                    update_streamer = Some((worker.streamer.clone(), full_specs.clone()));
                    upgrade_task = Some((
                        root.to_path_buf(),
                        worker.streamer.clone(),
                        worker.upgrade_lock.clone(),
                        primary_spec.model_id.clone(),
                        full_specs,
                    ));
                }
            }

            self.state.touch_lru(root);
            self.state.enforce_capacity(self.worker_capacity);

            if let Some((streamer, full_specs)) = update_streamer {
                let _ = streamer.set_models(full_specs).await;
            }

            if let Some((root_buf, streamer, lock, primary_model_id, full_specs)) = upgrade_task {
                spawn_polite_model_upgrade_task(
                    root_buf,
                    streamer,
                    lock,
                    primary_model_id,
                    full_specs,
                    false,
                );
            }
            return;
        }

        let root_buf = root.to_path_buf();
        if !self.state.starting.insert(root_buf.clone()) {
            // Another task is already starting a streamer for this root.
            return;
        }

        // No worker yet: create one outside the map to avoid keeping a half-initialized entry.
        let outcome = async {
            let indexer = match MultiModelProjectIndexer::new(&root_buf).await {
                Ok(indexer) => indexer,
                Err(_) => return None,
            };
            let streamer = match MultiModelStreamingIndexer::start(
                Arc::new(indexer),
                vec![primary_spec.clone()],
                self.cfg,
            ) {
                Ok(streamer) => streamer,
                Err(_) => return None,
            };
            Some(streamer)
        }
        .await;

        self.state.starting.remove(&root_buf);

        let Some(streamer) = outcome else {
            return;
        };

        self.state.workers.insert(
            root_buf.clone(),
            Worker::new(streamer.clone(), desired_model_ids, now, self.worker_ttl),
        );
        self.state.touch_lru(&root_buf);
        self.state.enforce_capacity(self.worker_capacity);

        let needs_bootstrap = should_bootstrap_primary(&root_buf, &primary_spec.model_id).await;
        if needs_bootstrap {
            // Cold-start or stale: immediately build the primary model index so semantic tools
            // become usable ASAP. This uses the primary-only model set, keeping the first cycle
            // as light as possible.
            let _ = streamer.trigger("bootstrap").await;
        }

        if desired_full_specs.len() > 1 {
            // Background: opportunistically upgrade missing/stale expert indices, but do so in a
            // "polite" way (delay + load-aware throttling) so the daemon remains cheap.
            let upgrade_lock = self
                .state
                .workers
                .get(&root_buf)
                .expect("worker inserted above")
                .upgrade_lock
                .clone();
            spawn_polite_model_upgrade_task(
                root_buf.clone(),
                streamer,
                upgrade_lock,
                primary_spec.model_id.clone(),
                desired_full_specs,
                needs_bootstrap,
            );
        }
    }

    pub async fn request_refresh(&mut self, root: &Path, reason: &str, model_ids: Vec<String>) {
        let now = Instant::now();
        self.state.prune_expired(now);

        let Some(worker) = self.state.workers.get_mut(root) else {
            return;
        };

        if now.duration_since(worker.last_refresh) < worker.refresh_debounce {
            return;
        }
        worker.last_refresh = now;

        let reason = encode_refresh_models_reason(reason, &model_ids);
        let _ = worker.streamer.trigger(reason).await;
    }
}

fn encode_refresh_models_reason(reason: &str, model_ids: &[String]) -> String {
    let mut ids: Vec<String> = model_ids
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();
    ids.sort();
    ids.dedup();

    if ids.is_empty() {
        return reason.to_string();
    }

    format!("{REFRESH_MODELS_REASON_PREFIX}{}:{reason}", ids.join(","))
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

fn index_path_for_model(root: &Path, model_id: &str) -> PathBuf {
    context_dir_for_project_root(root)
        .join("indexes")
        .join(model_id_dir_name(model_id))
        .join("index.json")
}

async fn should_bootstrap_primary(root: &Path, model_id: &str) -> bool {
    let store_path = index_path_for_model(root, model_id);
    if !store_path.exists() {
        return true;
    }
    let corpus_path = corpus_path_for_project_root(root);
    if !corpus_path.exists() {
        return true;
    }

    let project_watermark = match compute_project_watermark(root).await {
        Ok(mark) => mark,
        Err(_) => return true,
    };

    let stored = read_index_watermark(&store_path).await.ok().flatten();
    let index_watermark = stored.as_ref().map(|p| &p.watermark);
    assess_staleness(&project_watermark, true, false, index_watermark).stale
}

#[derive(Clone, Copy, Debug)]
struct PoliteLoadHint {
    load_per_cpu: Option<f64>,
    mem_available_gib: Option<u64>,
}

impl PoliteLoadHint {
    fn recommended_wait(&self) -> Duration {
        // Heuristic: if we cannot measure, default to "no wait".
        let Some(load) = self.load_per_cpu else {
            return Duration::from_secs(0);
        };

        // Memory pressure makes any background work more expensive (allocator churn, page cache
        // pressure). If we can detect low MemAvailable, be more conservative.
        let mem_low = self.mem_available_gib.is_some_and(|gib| gib <= 2);

        if load < 0.7 && !mem_low {
            Duration::from_secs(0)
        } else if load < 1.0 {
            Duration::from_secs(3)
        } else if load < 1.25 {
            Duration::from_secs(8)
        } else {
            Duration::from_secs(15)
        }
    }
}

fn polite_load_hint_linux_best_effort() -> PoliteLoadHint {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get() as f64)
        .unwrap_or(1.0)
        .max(1.0);

    let load_per_cpu = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|contents| contents.split_whitespace().next().map(str::to_string))
        .and_then(|v| v.parse::<f64>().ok())
        .map(|load1| load1 / cpus);

    let mem_available_gib = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|contents| {
            for line in contents.lines() {
                let line = line.trim_start();
                if !line.starts_with("MemAvailable:") {
                    continue;
                }
                let kb = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<u64>().ok())?;
                return Some(kb / 1024 / 1024);
            }
            None
        });

    PoliteLoadHint {
        load_per_cpu,
        mem_available_gib,
    }
}

async fn wait_for_polite_window(max_wait: Duration) {
    let started = Instant::now();
    tokio::time::sleep(POLITE_UPGRADE_INITIAL_DELAY).await;
    loop {
        let hint = polite_load_hint_linux_best_effort();
        let wait = hint.recommended_wait();
        if wait.is_zero() {
            return;
        }
        if started.elapsed() >= max_wait {
            return;
        }
        tokio::time::sleep(wait.min(max_wait)).await;
    }
}

fn spawn_polite_model_upgrade_task(
    root: PathBuf,
    streamer: MultiModelStreamingIndexer,
    upgrade_lock: Arc<TokioMutex<()>>,
    primary_model_id: String,
    desired_full_specs: Vec<ModelIndexSpec>,
    wait_for_bootstrap_success: bool,
) {
    tokio::spawn(async move {
        let Ok(_guard) = upgrade_lock.try_lock() else {
            return;
        };

        if wait_for_bootstrap_success {
            let mut updates = streamer.subscribe_updates();
            loop {
                match updates.recv().await {
                    Ok(update) if update.reason == "bootstrap" && update.success => break,
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
        }

        // Give interactive sessions a chance to settle before doing heavy work.
        wait_for_polite_window(POLITE_UPGRADE_MAX_WAIT).await;

        // Ensure the streamer knows about the full roster, but keep actual work targeted via
        // `refresh_models:*` reasons.
        let _ = streamer.set_models(desired_full_specs.clone()).await;

        let project_watermark = match compute_project_watermark(&root).await {
            Ok(mark) => mark,
            Err(err) => {
                log::debug!(
                    "warm index upgrade skipped for {}: failed to compute watermark: {err}",
                    root.display()
                );
                return;
            }
        };

        let mut upgrades: Vec<String> = desired_full_specs
            .iter()
            .map(|spec| spec.model_id.clone())
            .filter(|id| id != &primary_model_id)
            .collect();
        upgrades.sort();
        upgrades.dedup();

        if upgrades.is_empty() {
            return;
        }

        let mut updates = streamer.subscribe_updates();

        for model_id in upgrades {
            if !model_needs_refresh(&root, &project_watermark, &model_id).await {
                continue;
            }

            // Respect current system load: avoid kicking off a full embed pass while the machine
            // is already hot (builds/tests/other agents).
            wait_for_polite_window(POLITE_UPGRADE_MAX_WAIT).await;

            let reason = format!("{REFRESH_MODELS_REASON_PREFIX}{model_id}:warmup");
            let _ = streamer.trigger(reason.clone()).await;

            // Wait until the triggered cycle completes (or the receiver closes).
            loop {
                match updates.recv().await {
                    Ok(update) if update.reason == reason && update.success => break,
                    Ok(update) if update.reason == reason && !update.success => break,
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
        }
    });
}

async fn model_needs_refresh(root: &Path, project_watermark: &Watermark, model_id: &str) -> bool {
    let store_path = index_path_for_model(root, model_id);
    let index_exists = store_path.exists();

    let stored = read_index_watermark(&store_path).await.ok().flatten();
    let index_watermark = stored
        .as_ref()
        .map(|PersistedIndexWatermark { watermark, .. }| watermark);

    assess_staleness(project_watermark, index_exists, false, index_watermark).stale
}

fn worker_capacity_from_env() -> usize {
    std::env::var("CONTEXT_WARM_WORKER_CAPACITY")
        .or_else(|_| std::env::var("CONTEXT_FINDER_WARM_WORKER_CAPACITY"))
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(default_worker_capacity)
        .clamp(1, 64)
}

fn worker_ttl_from_env() -> Duration {
    std::env::var("CONTEXT_WARM_WORKER_TTL_SECS")
        .or_else(|_| std::env::var("CONTEXT_FINDER_WARM_WORKER_TTL_SECS"))
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(default_worker_ttl)
        .clamp(Duration::from_secs(30), Duration::from_secs(60 * 60))
}

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

fn default_worker_capacity() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let Some(mem_gib) = total_memory_gib_linux_best_effort() else {
        // Conservative: keep background watchers bounded without knowing memory size.
        return if cpus <= 4 { 2 } else { 4 };
    };

    if mem_gib <= 8 {
        2
    } else if mem_gib <= 16 {
        3
    } else if mem_gib <= 32 {
        5
    } else if cpus >= 16 {
        10
    } else {
        8
    }
}

fn default_worker_ttl() -> Duration {
    let Some(mem_gib) = total_memory_gib_linux_best_effort() else {
        return Duration::from_secs(5 * 60);
    };

    if mem_gib <= 8 {
        Duration::from_secs(2 * 60)
    } else if mem_gib <= 16 {
        Duration::from_secs(5 * 60)
    } else {
        Duration::from_secs(10 * 60)
    }
}

fn model_specs(
    model_ids: Vec<String>,
    templates: context_vector_store::EmbeddingTemplates,
) -> (ModelIndexSpec, Vec<ModelIndexSpec>, Vec<String>) {
    let mut set: HashSet<String> = HashSet::new();
    for id in model_ids {
        let trimmed = id.trim();
        if trimmed.is_empty() {
            continue;
        }
        set.insert(trimmed.to_string());
    }

    let primary_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    set.insert(primary_id.clone());

    let mut ids: Vec<String> = set.into_iter().collect();
    ids.sort();

    let full_specs: Vec<ModelIndexSpec> = ids
        .iter()
        .map(|id| ModelIndexSpec::new(id.clone(), templates.clone()))
        .collect();

    let primary_spec = full_specs
        .iter()
        .find(|spec| spec.model_id == primary_id)
        .cloned()
        .unwrap_or_else(|| full_specs.first().cloned().unwrap());

    (primary_spec, full_specs, ids)
}
