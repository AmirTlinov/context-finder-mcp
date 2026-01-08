use crate::{IndexStats, Result};
use context_vector_store::{context_dir_for_project_root, current_model_id};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;

const MAX_FAILURES: usize = 5;

/// Snapshot persisted to `.context/health.json` so other processes can
/// report the last successful indexing run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub last_success_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p95_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_per_sec: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_events: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_indexed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunks_indexed: Option<usize>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_reasons: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_cache_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_count: Option<usize>,
}

pub async fn write_health_snapshot(
    root: &Path,
    stats: &IndexStats,
    reason: &str,
    p95_duration_ms: Option<u64>,
    pending_events: Option<usize>,
) -> Result<HealthSnapshot> {
    let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    let index_path = context_dir_for_project_root(root)
        .join("indexes")
        .join(model_id_dir_name(&model_id))
        .join("index.json");
    let index_size_bytes = tokio::fs::metadata(index_path).await.ok().map(|m| m.len());
    let graph_cache_size_bytes = tokio::fs::metadata(
        context_dir_for_project_root(root).join("graph_cache.json"),
    )
    .await
    .ok()
    .map(|m| m.len());
    let files_per_sec = if stats.time_ms > 0 {
        #[allow(clippy::cast_precision_loss)]
        Some(stats.files as f32 / (stats.time_ms as f32 / 1000.0))
    } else {
        None
    };
    let snapshot = HealthSnapshot {
        last_success_unix_ms: current_unix_ms(),
        last_duration_ms: Some(stats.time_ms),
        p95_duration_ms,
        files_per_sec,
        pending_events,
        files_indexed: Some(stats.files),
        chunks_indexed: Some(stats.chunks),
        reason: reason.to_string(),
        failure_reasons: Vec::new(),
        last_failure_unix_ms: None,
        last_failure_reason: None,
        index_size_bytes,
        graph_cache_size_bytes,
        failure_count: Some(0),
    };

    let path = health_file_path(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let data = serde_json::to_vec_pretty(&snapshot)?;
    fs::write(&path, data).await?;
    Ok(snapshot)
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

pub async fn append_failure_reason(
    root: &Path,
    reason: &str,
    detail: &str,
    p95_duration_ms: Option<u64>,
) -> Result<()> {
    let mut snapshot = read_health_snapshot(root)
        .await?
        .unwrap_or_else(|| HealthSnapshot {
            last_success_unix_ms: 0,
            last_duration_ms: None,
            p95_duration_ms: None,
            files_indexed: None,
            chunks_indexed: None,
            reason: "failure".to_string(),
            failure_reasons: Vec::new(),
            last_failure_unix_ms: None,
            last_failure_reason: None,
            index_size_bytes: None,
            graph_cache_size_bytes: None,
            failure_count: None,
            files_per_sec: None,
            pending_events: None,
        });

    snapshot.failure_reasons.push(format!("{reason}: {detail}"));
    snapshot.p95_duration_ms = snapshot.p95_duration_ms.or(p95_duration_ms);
    snapshot.last_failure_unix_ms = Some(current_unix_ms());
    snapshot.last_failure_reason = Some(detail.to_string());
    if snapshot.failure_reasons.len() > MAX_FAILURES {
        let start = snapshot.failure_reasons.len() - MAX_FAILURES;
        snapshot.failure_reasons = snapshot.failure_reasons.split_off(start);
    }
    snapshot.failure_count = Some(snapshot.failure_reasons.len());

    let path = health_file_path(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let data = serde_json::to_vec_pretty(&snapshot)?;
    fs::write(&path, data).await?;
    Ok(())
}

pub async fn read_health_snapshot(root: &Path) -> Result<Option<HealthSnapshot>> {
    let path = health_file_path(root);
    match fs::read(&path).await {
        Ok(bytes) => {
            let snapshot = serde_json::from_slice(&bytes)?;
            Ok(Some(snapshot))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

#[must_use]
pub fn health_file_path(root: &Path) -> PathBuf {
    context_dir_for_project_root(root).join("health.json")
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|dur| u64::try_from(dur.as_millis()).ok())
        .unwrap_or(0)
}
