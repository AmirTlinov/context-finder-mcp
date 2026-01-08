use crate::command::context::index_path;
use crate::command::domain::{CommandOutcome, Hint, HintKind};
use anyhow::Result;
use context_indexer::{read_health_snapshot, write_health_snapshot, HealthSnapshot, IndexStats};
use context_vector_store::context_dir_for_project_root;
use serde::Serialize;
use std::path::Path;
use tokio::fs;

#[derive(Clone, Default)]
pub struct HealthPort;

#[derive(Debug, Serialize)]
pub struct HealthReport {
    pub status: String,
    pub last_success_unix_ms: Option<u64>,
    pub last_failure_unix_ms: Option<u64>,
    pub p95_duration_ms: Option<u64>,
    pub files_per_sec: Option<f32>,
    pub pending_events: Option<usize>,
    #[serde(default)]
    pub failures: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_cache_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_ms: Option<u64>,
}

impl HealthPort {
    pub async fn record_index(
        &self,
        root: &Path,
        stats: &IndexStats,
        reason: &str,
    ) -> Result<HealthSnapshot> {
        write_health_snapshot(root, stats, reason, None, None)
            .await
            .map_err(Into::into)
    }

    pub async fn attach(&self, root: &Path, outcome: &mut CommandOutcome) {
        match read_health_snapshot(root).await {
            Ok(Some(snapshot)) => add_snapshot(snapshot, outcome),
            Ok(None) => {}
            Err(err) => {
                log::warn!("Health snapshot read failed: {err}");
            }
        }
    }

    pub async fn probe(&self, root: &Path) -> Result<HealthReport> {
        let snapshot = read_health_snapshot(root).await.ok().flatten();
        let index_path = index_path(root);
        let graph_path = context_dir_for_project_root(root).join("graph_cache.json");

        let index_size_bytes = fs::metadata(&index_path).await.ok().map(|m| m.len());
        let graph_cache_size_bytes = fs::metadata(&graph_path).await.ok().map(|m| m.len());

        let snapshot_ref = snapshot.as_ref();
        let failures = snapshot_ref
            .map(|s| s.failure_reasons.clone())
            .unwrap_or_default();
        let stale_ms =
            snapshot_ref.map(|s| current_unix_ms().saturating_sub(s.last_success_unix_ms));
        Ok(HealthReport {
            status: if snapshot.is_some() { "ok" } else { "cold" }.to_string(),
            last_success_unix_ms: snapshot_ref.map(|s| s.last_success_unix_ms),
            last_failure_unix_ms: snapshot_ref.and_then(|s| s.last_failure_unix_ms),
            p95_duration_ms: snapshot_ref.and_then(|s| s.p95_duration_ms),
            files_per_sec: snapshot_ref.and_then(|s| s.files_per_sec),
            pending_events: snapshot_ref.and_then(|s| s.pending_events),
            failures,
            last_failure_reason: snapshot_ref.and_then(|s| s.last_failure_reason.clone()),
            index_size_bytes,
            graph_cache_size_bytes,
            failure_count: snapshot_ref.and_then(|s| s.failure_count),
            stale_ms,
        })
    }
}

fn add_snapshot(snapshot: HealthSnapshot, outcome: &mut CommandOutcome) {
    outcome.meta.health_last_success_ms = Some(snapshot.last_success_unix_ms);
    outcome.meta.index_files = snapshot.files_indexed;
    outcome.meta.index_chunks = snapshot.chunks_indexed;
    outcome.meta.health_last_failure_ms = snapshot.last_failure_unix_ms;
    if !snapshot.failure_reasons.is_empty() {
        outcome.meta.health_failure_reasons = Some(snapshot.failure_reasons.clone());
    }
    outcome.meta.health_p95_ms = snapshot.p95_duration_ms;
    if let Some(count) = snapshot.failure_count {
        outcome.meta.health_failure_count = Some(count);
    }
    outcome.meta.health_files_per_sec = snapshot.files_per_sec;
    outcome.meta.health_pending_events = snapshot.pending_events;
    outcome.meta.index_size_bytes = outcome.meta.index_size_bytes.or(snapshot.index_size_bytes);
    outcome.meta.graph_cache_size_bytes = outcome
        .meta
        .graph_cache_size_bytes
        .or(snapshot.graph_cache_size_bytes);
    outcome.hints.push(Hint {
        kind: HintKind::Info,
        text: format!(
            "Watcher/index last success at {} ms (reason: {})",
            snapshot.last_success_unix_ms, snapshot.reason
        ),
    });
    if !snapshot.failure_reasons.is_empty() {
        outcome.hints.push(Hint {
            kind: HintKind::Warn,
            text: format!(
                "Recent indexing failures: {}",
                snapshot.failure_reasons.join("; ")
            ),
        });
    }
    if let Some(ts) = snapshot.last_failure_unix_ms {
        outcome.hints.push(Hint {
            kind: HintKind::Warn,
            text: if let Some(reason) = snapshot.last_failure_reason.as_ref() {
                format!("Last failure at {} ms: {}", ts, reason)
            } else {
                format!("Last failure at {} ms", ts)
            },
        });
    }
    if let Some(p95) = snapshot.p95_duration_ms {
        outcome.hints.push(Hint {
            kind: HintKind::Info,
            text: format!("Index p95 duration over recent runs: {} ms", p95),
        });
        const P95_WARN_MS: u64 = 2_000;
        if p95 > P95_WARN_MS {
            outcome.hints.push(Hint {
                kind: HintKind::Warn,
                text: format!(
                    "Indexing p95 is high ({} ms), consider re-running index or reducing backlog",
                    p95
                ),
            });
        }
    }
    let stale_ms = current_unix_ms().saturating_sub(snapshot.last_success_unix_ms);
    outcome.meta.health_stale_ms = Some(stale_ms);
    const STALE_WARN_MS: u64 = 15 * 60 * 1000; // 15 minutes
    if stale_ms > STALE_WARN_MS {
        outcome.hints.push(Hint {
            kind: HintKind::Warn,
            text: format!("Index may be stale (last success {} ms ago)", stale_ms),
        });
    }
    if let Some(pending) = snapshot.pending_events {
        const PENDING_WARN: usize = 50;
        if pending > PENDING_WARN {
            outcome.hints.push(Hint {
                kind: HintKind::Warn,
                text: format!("Watcher backlog: {} pending fs events", pending),
            });
        } else if pending > 0 {
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: format!("Watcher backlog pending events: {}", pending),
            });
        }
    }
    if let Some(fps) = snapshot.files_per_sec {
        const FPS_WARN: f32 = 0.5;
        if fps < FPS_WARN {
            outcome.hints.push(Hint {
                kind: HintKind::Warn,
                text: format!("Indexing throughput low: {:.2} files/s", fps),
            });
        }
    }
    outcome.meta.health_last_failure_reason = snapshot.last_failure_reason.clone();
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
