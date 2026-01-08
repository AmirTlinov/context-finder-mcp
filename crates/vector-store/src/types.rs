use context_code_chunker::CodeChunk;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredChunk {
    pub chunk: CodeChunk,
    pub vector: Arc<Vec<f32>>,
    pub id: String,
    #[serde(default)]
    pub doc_hash: u64,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub chunk: CodeChunk,
    pub score: f32,
    pub id: String,
}
