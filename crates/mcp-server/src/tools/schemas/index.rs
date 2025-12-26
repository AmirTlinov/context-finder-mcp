use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IndexRequest {
    /// Project directory path
    #[schemars(
        description = "Project directory to index (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

    /// Force full reindex (alias for `full`)
    #[schemars(description = "Force full reindex ignoring cache (alias for `full`)")]
    pub force: Option<bool>,

    /// Index expert roster models from the active profile (opt-in)
    #[schemars(
        description = "If true, index the profile's expert roster models in addition to the primary model"
    )]
    pub experts: Option<bool>,

    /// Additional model IDs to index (opt-in)
    #[schemars(description = "Additional model IDs to index")]
    pub models: Option<Vec<String>>,

    /// Full reindex (skip incremental checks)
    #[schemars(description = "Run a full reindex (skip incremental checks)")]
    pub full: Option<bool>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct IndexResult {
    /// Number of files indexed
    pub files: usize,
    /// Number of chunks created
    pub chunks: usize,
    /// Indexing time in milliseconds
    pub time_ms: u64,
    /// Index file path
    pub index_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
}
