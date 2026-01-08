use anyhow::{Context, Result};
use context_indexer::{
    ModelIndexSpec, MultiModelProjectIndexer, MultiModelStreamingIndexer, StreamingIndexerConfig,
};
use context_search::SearchProfile;
use context_vector_store::{
    current_model_id, QueryKind, CONTEXT_DIR_NAME, LEGACY_CONTEXT_DIR_NAME,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{watch, Mutex};

const DEFAULT_TTL: Duration = Duration::from_secs(300);
const DEFAULT_CLEANUP_INTERVAL: Duration = Duration::from_secs(30);
const MAX_DAEMON_LINE_BYTES: usize = if cfg!(test) { 1024 } else { 1024 * 1024 };

async fn read_line_limited<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = reader.read(&mut tmp).await?;
        if n == 0 {
            break;
        }

        if let Some(pos) = tmp[..n].iter().position(|b| *b == b'\n') {
            if out.len() + pos > max_bytes {
                anyhow::bail!("daemon message exceeds {max_bytes} bytes");
            }
            out.extend_from_slice(&tmp[..pos]);
            break;
        }

        if out.len() + n > max_bytes {
            anyhow::bail!("daemon message exceeds {max_bytes} bytes");
        }
        out.extend_from_slice(&tmp[..n]);
    }

    if out.last() == Some(&b'\r') {
        out.pop();
    }
    Ok(out)
}

fn duration_from_env_ms(var: &str) -> Option<Duration> {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
}

fn daemon_ttl() -> Duration {
    duration_from_env_ms("CONTEXT_DAEMON_TTL_MS")
        .or_else(|| duration_from_env_ms("CONTEXT_FINDER_DAEMON_TTL_MS"))
        .unwrap_or(DEFAULT_TTL)
}

fn daemon_cleanup_interval() -> Duration {
    duration_from_env_ms("CONTEXT_DAEMON_CLEANUP_MS")
        .or_else(|| duration_from_env_ms("CONTEXT_FINDER_DAEMON_CLEANUP_MS"))
        .unwrap_or(DEFAULT_CLEANUP_INTERVAL)
}

#[derive(Serialize, Deserialize)]
struct PingRequest {
    cmd: String,
    project: String,
    #[serde(default)]
    ttl_ms: Option<u64>,
    #[serde(default)]
    models: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize)]
struct PingResponse {
    status: String,
    message: Option<String>,
}

#[derive(Serialize)]
struct StatusResponse {
    status: String,
    message: Option<String>,
    projects: Vec<StatusProject>,
}

#[derive(Serialize)]
struct StatusProject {
    project: String,
    age_ms: u64,
    ttl_ms: u64,
}

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

fn default_socket_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let preferred = home.join(CONTEXT_DIR_NAME);
    let base = if preferred.exists() {
        preferred
    } else {
        let legacy = home.join(LEGACY_CONTEXT_DIR_NAME);
        if legacy.exists() {
            legacy
        } else {
            preferred
        }
    };
    base.join(format!("daemon.{}.sock", exe_build_id_best_effort()))
}

fn exe_build_id_best_effort() -> String {
    let exe = std::env::current_exe().ok();
    let build_id = exe.as_deref().and_then(exe_build_id_from_path_best_effort);
    build_id.unwrap_or_else(|| "default".to_string())
}

fn exe_build_id_from_path_best_effort(exe: &Path) -> Option<String> {
    let meta = std::fs::metadata(exe).ok()?;
    let len = meta.len();
    let modified = meta.modified().ok()?;
    let modified_ms = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_millis();
    let modified_ms = u64::try_from(modified_ms).unwrap_or(u64::MAX);
    Some(format!("{len:x}-{modified_ms:x}"))
}

pub async fn run_daemon(socket: Option<PathBuf>) -> Result<()> {
    let socket_path = socket.unwrap_or_else(default_socket_path);
    let listener = match bind_single_instance(&socket_path).await? {
        Some(listener) => listener,
        None => return Ok(()), // another daemon is already running
    };

    let ttl = daemon_ttl();
    let cleanup_interval = daemon_cleanup_interval();

    let shared = std::sync::Arc::new(DaemonShared::from_env().await?);
    let state = std::sync::Arc::new(Mutex::new(HashMap::<PathBuf, Worker>::new()));
    let last_activity = std::sync::Arc::new(Mutex::new(Instant::now()));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let listener_state = state.clone();
    let listener_shared = shared.clone();
    let listener_activity = last_activity.clone();

    // cleanup task
    tokio::spawn({
        let state = state.clone();
        let last_activity = last_activity.clone();
        let shutdown_tx = shutdown_tx.clone();
        async move {
            loop {
                tokio::time::sleep(cleanup_interval).await;
                let now = Instant::now();

                let empty = {
                    let mut guard = state.lock().await;
                    guard.retain(|_, w| now.duration_since(w.last_ping) < w.ttl);
                    guard.is_empty()
                };

                if empty {
                    let last = *last_activity.lock().await;
                    if now.duration_since(last) >= ttl {
                        let _ = shutdown_tx.send(true);
                        break;
                    }
                }
            }
        }
    });

    let mut shutdown_rx = shutdown_rx;
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            res = listener.accept() => {
                let (stream, _) = res?;
                let st = listener_state.clone();
                let shared = listener_shared.clone();
                let activity = listener_activity.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_conn(stream, st, shared, activity).await {
                        log::warn!("daemon connection error: {err:#}");
                    }
                });
            }
        }
    }

    let _ = tokio::fs::remove_file(&socket_path).await;
    Ok(())
}

async fn handle_conn(
    stream: UnixStream,
    state: std::sync::Arc<Mutex<HashMap<PathBuf, Worker>>>,
    shared: std::sync::Arc<DaemonShared>,
    last_activity: std::sync::Arc<Mutex<Instant>>,
) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let line = read_line_limited(&mut reader, MAX_DAEMON_LINE_BYTES).await?;
    let req: PingRequest = serde_json::from_slice(&line)?;
    *last_activity.lock().await = Instant::now();

    let resp_json = match req.cmd.as_str() {
        "ping" => {
            let ttl = req
                .ttl_ms
                .map(Duration::from_millis)
                .unwrap_or_else(daemon_ttl);
            let project = PathBuf::from(req.project);

            let requested_models = req
                .models
                .as_ref()
                .map(|v| {
                    v.iter()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                })
                .filter(|v| !v.is_empty());

            let (primary_spec, full_specs) = shared.specs_for_request(requested_models.as_deref());
            let desired_model_ids: Vec<String> = full_specs
                .iter()
                .map(|spec| spec.model_id.clone())
                .collect();

            let mut existing_streamer: Option<MultiModelStreamingIndexer> = None;
            let mut existing_models: Option<Vec<String>> = None;

            {
                let mut guard = state.lock().await;
                if let Some(w) = guard.get_mut(&project) {
                    w.ttl = ttl;
                    w.last_ping = Instant::now();
                    existing_streamer = Some(w.streamer.clone());
                    existing_models = Some(w.models.clone());
                }
            }

            if let Some(streamer) = existing_streamer {
                let should_update_models = requested_models.is_some()
                    && existing_models
                        .as_ref()
                        .is_none_or(|m| m != &desired_model_ids);
                if should_update_models && streamer.set_models(full_specs.clone()).await.is_ok() {
                    let _ = streamer.trigger("models_changed").await;
                    let mut guard = state.lock().await;
                    if let Some(w) = guard.get_mut(&project) {
                        w.models = desired_model_ids;
                    }
                }
            } else {
                let indexer = MultiModelProjectIndexer::new(&project).await?;
                let cfg = StreamingIndexerConfig {
                    max_batch_wait: Duration::from_secs(2),
                    ..Default::default()
                };

                // Cold-start optimization: index only the primary model first to get a usable
                // semantic index ASAP, then expand to the full roster in the background.
                let initial_specs = vec![primary_spec.clone()];
                let streamer = MultiModelStreamingIndexer::start(
                    std::sync::Arc::new(indexer),
                    initial_specs,
                    cfg,
                )?;
                let mut worker = Worker::new(streamer.clone(), desired_model_ids.clone());
                worker.ttl = ttl;
                worker.last_ping = Instant::now();
                {
                    let mut guard = state.lock().await;
                    guard.insert(project.clone(), worker);
                }

                // Trigger immediate incremental index to warm.
                let _ = streamer.trigger("bootstrap").await;

                if full_specs.len() > 1 {
                    let streamer = streamer.clone();
                    tokio::spawn(async move {
                        let mut updates = streamer.subscribe_updates();
                        loop {
                            match updates.recv().await {
                                Ok(update) => {
                                    if update.success {
                                        break;
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    continue;
                                }
                                Err(_) => return,
                            }
                        }
                        if streamer.set_models(full_specs).await.is_ok() {
                            let _ = streamer.trigger("upgrade_models").await;
                        }
                    });
                }
            }

            serde_json::to_string(&PingResponse {
                status: "ok".to_string(),
                message: None,
            })?
        }
        "status" => {
            let now = Instant::now();
            let projects = {
                let guard = state.lock().await;
                let mut out = Vec::with_capacity(guard.len());
                for (path, worker) in guard.iter() {
                    out.push(StatusProject {
                        project: path.to_string_lossy().to_string(),
                        age_ms: now.duration_since(worker.last_ping).as_millis() as u64,
                        ttl_ms: worker.ttl.as_millis() as u64,
                    });
                }
                out
            };
            serde_json::to_string(&StatusResponse {
                status: "ok".to_string(),
                message: None,
                projects,
            })?
        }
        other => serde_json::to_string(&PingResponse {
            status: "error".to_string(),
            message: Some(format!("unknown command '{other}'")),
        })?,
    };

    let mut writer = reader.into_inner();
    writer.write_all(resp_json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

struct Worker {
    streamer: MultiModelStreamingIndexer,
    models: Vec<String>,
    ttl: Duration,
    last_ping: Instant,
}

impl Worker {
    fn new(streamer: MultiModelStreamingIndexer, models: Vec<String>) -> Self {
        Self {
            streamer,
            models,
            ttl: daemon_ttl(),
            last_ping: Instant::now(),
        }
    }
}

struct DaemonShared {
    primary_spec: ModelIndexSpec,
    full_specs: Vec<ModelIndexSpec>,
    installed_model_ids: Option<HashSet<String>>,
}

impl DaemonShared {
    async fn from_env() -> Result<Self> {
        let profile = load_profile_from_env();
        let templates = profile.embedding().clone();

        let model_dir = context_vector_store::model_dir();
        let installed = load_installed_model_ids(&model_dir).await.ok();

        let mut models = HashSet::new();
        let primary = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
        models.insert(primary);

        let experts = profile.experts();
        for kind in [
            QueryKind::Identifier,
            QueryKind::Path,
            QueryKind::Conceptual,
        ] {
            for model_id in experts.semantic_models(kind) {
                models.insert(model_id.clone());
            }
        }

        let mut model_ids: Vec<String> = models.into_iter().collect();
        model_ids.sort();

        if let Some(installed) = installed.as_ref() {
            model_ids.retain(|id| installed.contains(id));
        }

        if model_ids.is_empty() {
            model_ids.push("bge-small".to_string());
        }

        let full_specs: Vec<ModelIndexSpec> = model_ids
            .into_iter()
            .map(|model_id| ModelIndexSpec::new(model_id, templates.clone()))
            .collect();

        let primary_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
        let primary_spec = full_specs
            .iter()
            .find(|spec| spec.model_id == primary_id)
            .cloned()
            .unwrap_or_else(|| full_specs.first().cloned().unwrap());

        Ok(Self {
            primary_spec,
            full_specs,
            installed_model_ids: installed,
        })
    }

    fn specs_for_request(
        &self,
        requested_models: Option<&[String]>,
    ) -> (ModelIndexSpec, Vec<ModelIndexSpec>) {
        let templates = self.primary_spec.templates.clone();

        let mut ids: Vec<String> = if let Some(requested) = requested_models {
            let mut set = HashSet::new();
            for id in requested {
                let trimmed = id.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }
                set.insert(trimmed);
            }
            set.insert(self.primary_spec.model_id.clone());
            let mut out: Vec<String> = set.into_iter().collect();
            out.sort();
            out
        } else {
            self.full_specs.iter().map(|s| s.model_id.clone()).collect()
        };

        if let Some(installed) = self.installed_model_ids.as_ref() {
            ids.retain(|id| installed.contains(id));
        }

        if ids.is_empty() {
            ids.push(self.primary_spec.model_id.clone());
        }

        let full_specs: Vec<ModelIndexSpec> = ids
            .iter()
            .map(|model_id| ModelIndexSpec::new(model_id.clone(), templates.clone()))
            .collect();

        let primary_spec = full_specs
            .iter()
            .find(|spec| spec.model_id == self.primary_spec.model_id)
            .cloned()
            .unwrap_or_else(|| full_specs.first().cloned().unwrap());

        (primary_spec, full_specs)
    }
}

pub async fn ping(project: &Path) -> Result<()> {
    // In stub embedding mode we aim for deterministic, dependency-light behavior
    // (used heavily in CI and tests). The background daemon is a performance
    // optimization and can introduce nondeterminism / flakiness in constrained
    // environments, so we skip it.
    if std::env::var("CONTEXT_EMBEDDING_MODE")
        .or_else(|_| std::env::var("CONTEXT_FINDER_EMBEDDING_MODE"))
        .ok()
        .map(|v| v.trim().eq_ignore_ascii_case("stub"))
        .unwrap_or(false)
    {
        return Ok(());
    }

    let socket = default_socket_path();
    ensure_daemon(&socket).await?;
    let ttl = daemon_ttl();
    let models = desired_model_ids_from_env();
    let payload = PingRequest {
        cmd: "ping".to_string(),
        project: project.to_string_lossy().to_string(),
        ttl_ms: Some(ttl.as_millis() as u64),
        models: Some(models),
    };
    let resp = send_ping(&socket, &payload).await;
    match resp {
        Ok(_) => Ok(()),
        Err(_) => {
            // maybe daemon died, restart once
            ensure_daemon(&socket).await?;
            send_ping(&socket, &payload).await?;
            Ok(())
        }
    }
}

fn desired_model_ids_from_env() -> Vec<String> {
    let profile = load_profile_from_env();
    let mut models = HashSet::new();

    let primary = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    models.insert(primary);

    let experts = profile.experts();
    for kind in [
        QueryKind::Identifier,
        QueryKind::Path,
        QueryKind::Conceptual,
    ] {
        for model_id in experts.semantic_models(kind) {
            models.insert(model_id.clone());
        }
    }

    let mut out: Vec<String> = models.into_iter().collect();
    out.sort();
    out
}

async fn send_ping(socket: &Path, payload: &PingRequest) -> Result<()> {
    let mut stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to daemon at {}", socket.display()))?;
    let msg = serde_json::to_string(payload)? + "\n";
    stream.write_all(msg.as_bytes()).await?;
    stream.flush().await?;
    let mut reader = BufReader::new(stream);
    let line = read_line_limited(&mut reader, MAX_DAEMON_LINE_BYTES).await?;
    let resp: PingResponse = serde_json::from_slice(&line)?;
    if resp.status == "ok" {
        Ok(())
    } else {
        anyhow::bail!(resp.message.unwrap_or_else(|| "daemon error".to_string()))
    }
}

async fn ensure_daemon(socket: &Path) -> Result<()> {
    if UnixStream::connect(socket).await.is_ok() {
        return Ok(());
    }
    // spawn daemon
    let exe = std::env::current_exe()?;
    tokio::process::Command::new(exe)
        .arg("daemon-loop")
        .arg("--socket")
        .arg(socket)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| "failed to spawn daemon-loop")?;
    // wait for socket to appear
    let mut retries = 0;
    while retries < 20 {
        if UnixStream::connect(socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        retries += 1;
    }
    anyhow::bail!("daemon did not start in time")
}

async fn bind_single_instance(socket_path: &Path) -> Result<Option<UnixListener>> {
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    match UnixListener::bind(socket_path) {
        Ok(listener) => return Ok(Some(listener)),
        Err(err) => {
            // If connect succeeds, another daemon is already listening.
            if UnixStream::connect(socket_path).await.is_ok() {
                return Ok(None);
            }
            log::debug!(
                "daemon socket bind failed ({}), treating as stale and retrying: {err}",
                socket_path.display()
            );
        }
    }

    let _ = tokio::fs::remove_file(socket_path).await;
    Ok(Some(UnixListener::bind(socket_path).with_context(
        || format!("failed to bind {}", socket_path.display()),
    )?))
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

async fn load_installed_model_ids(model_dir: &Path) -> Result<HashSet<String>> {
    let manifest_path = model_dir.join("manifest.json");
    if !manifest_path.exists() {
        return Ok(HashSet::new());
    }
    let bytes = tokio::fs::read(&manifest_path)
        .await
        .with_context(|| format!("Failed to read model manifest {}", manifest_path.display()))?;
    let parsed: ModelManifestFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("Failed to parse model manifest {}", manifest_path.display()))?;

    let mut installed = HashSet::new();
    for model in parsed.models {
        let mut missing = false;
        for asset in model.assets {
            let full = crate::models::safe_join_asset_path(model_dir, &asset.path)?;
            if !full.exists() {
                missing = true;
                break;
            }
        }
        if !missing {
            installed.insert(model.id);
        }
    }
    Ok(installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_line_limited_rejects_oversized_messages() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let handle = tokio::spawn(async move {
            let payload = vec![b'a'; MAX_DAEMON_LINE_BYTES + 1];
            client.write_all(&payload).await.expect("write");
            client.write_all(b"\n").await.expect("write newline");
            client.flush().await.expect("flush");
        });

        let err = read_line_limited(&mut server, MAX_DAEMON_LINE_BYTES)
            .await
            .expect_err("expected size limit error");
        assert!(
            err.to_string().contains("exceeds"),
            "unexpected error: {err}"
        );

        handle.await.expect("writer task");
    }
}
