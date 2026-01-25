use context_indexer::ToolMeta;
use context_protocol::ToolNextAction;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;
use crate::runtime_env;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DoctorRequest {
    /// Project directory path (optional)
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_ROOT/CONTEXT_PROJECT_ROOT; non-daemon fallback: cwd)."
    )]
    pub path: Option<String>,

    /// Response mode:
    /// - "facts" (default): keeps meta/index_state for freshness, strips next_actions to reduce noise.
    /// - "full": includes meta/index_state and next_actions (when applicable).
    /// - "minimal": strips index_state and next_actions, but keeps provenance meta (`root_fingerprint`).
    #[schemars(description = "Response mode: 'facts' (default), 'full', or 'minimal'")]
    pub response_mode: Option<ResponseMode>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DoctorResult {
    pub env: DoctorEnvResult,
    pub project: Option<DoctorProjectResult>,
    pub issues: Vec<String>,
    pub hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_actions: Vec<ToolNextAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observability: Option<DoctorObservability>,
    #[serde(default)]
    pub meta: ToolMeta,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DoctorObservability {
    pub indexing: DoctorIndexingObservability,
    pub warm_indexers: DoctorWarmIndexersObservability,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DoctorIndexingObservability {
    /// Configured max parallel indexing operations (per process).
    pub concurrency_limit: usize,
    /// In-flight indexing operations holding a permit (best-effort).
    pub concurrency_in_flight: usize,
    /// Indexing operations currently waiting for a permit (best-effort).
    pub concurrency_waiters: usize,
    /// Last observed index write-lock wait in milliseconds (best-effort).
    pub write_lock_wait_ms_last: u64,
    /// Max observed index write-lock wait in milliseconds (best-effort).
    pub write_lock_wait_ms_max: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DoctorWarmIndexersObservability {
    /// Number of warm streaming index workers currently cached.
    pub workers: usize,
    /// Number of roots currently starting a warm worker.
    pub starting: usize,
    /// LRU size for cached warm workers.
    pub lru: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DoctorEnvResult {
    pub profile: String,
    pub model_dir: String,
    pub model_manifest_exists: bool,
    pub models: Vec<DoctorModelStatus>,
    pub gpu: runtime_env::GpuEnvReport,
    pub cuda_disabled: bool,
    pub allow_cpu_fallback: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DoctorModelStatus {
    pub id: String,
    pub installed: bool,
    pub missing_assets: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DoctorIndexDrift {
    pub model: String,
    pub index_path: String,
    pub index_chunks: usize,
    pub corpus_chunks: usize,
    pub missing_chunks: usize,
    pub extra_chunks: usize,
    pub missing_file_samples: Vec<String>,
    pub extra_file_samples: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DoctorProjectResult {
    pub root: String,
    pub corpus_path: String,
    pub has_corpus: bool,
    pub indexed_models: Vec<String>,
    pub drift: Vec<DoctorIndexDrift>,
}
