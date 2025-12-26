use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::runtime_env;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DoctorRequest {
    /// Project directory path (optional)
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DoctorResult {
    pub env: DoctorEnvResult,
    pub project: Option<DoctorProjectResult>,
    pub issues: Vec<String>,
    pub hints: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
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
