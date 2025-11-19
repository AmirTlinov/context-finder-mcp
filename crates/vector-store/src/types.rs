use context_code_chunker::CodeChunk;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredChunk {
    pub chunk: CodeChunk,
    pub vector: Vec<f32>,
    pub id: String,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub chunk: CodeChunk,
    pub score: f32,
    pub id: String,
}
