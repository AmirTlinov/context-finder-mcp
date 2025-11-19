use crate::embeddings::EmbeddingModel;
use crate::error::Result;
use crate::hnsw_index::HnswIndex;
use crate::types::{SearchResult, StoredChunk};
use context_code_chunker::CodeChunk;
use std::collections::HashMap;
use std::path::Path;

pub struct VectorStore {
    chunks: HashMap<String, StoredChunk>,
    index: HnswIndex,
    embedder: EmbeddingModel,
    path: std::path::PathBuf,
    next_id: usize,
    id_map: HashMap<usize, String>, // numeric_id -> string_id mapping
}

impl VectorStore {
    pub async fn new(path: impl AsRef<Path>) -> Result<Self> {
        log::info!("Initializing VectorStore at {:?}", path.as_ref());
        let embedder = EmbeddingModel::new().await?;
        let dimension = embedder.dimension();
        let index = HnswIndex::new(dimension);

        Ok(Self {
            chunks: HashMap::new(),
            index,
            embedder,
            path: path.as_ref().to_path_buf(),
            next_id: 0,
            id_map: HashMap::new(),
        })
    }

    /// Add chunks with batch embedding for efficiency
    pub async fn add_chunks(&mut self, chunks: Vec<CodeChunk>) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }

        log::info!("Adding {} chunks to store", chunks.len());

        // Extract content for batch embedding
        let contents: Vec<&str> = chunks.iter().map(|c| c.content.as_str()).collect();

        // Batch embed for efficiency (much faster than one-by-one)
        let vectors = self.embedder.embed_batch(contents).await?;

        // Store chunks with their vectors
        for (chunk, vector) in chunks.into_iter().zip(vectors.into_iter()) {
            let id = format!("{}:{}:{}", chunk.file_path, chunk.start_line, chunk.end_line);
            let numeric_id = self.next_id;
            self.next_id += 1;

            // Add to HNSW index
            self.index.add(numeric_id, &vector)?;

            // Add to id mapping
            self.id_map.insert(numeric_id, id.clone());

            let stored = StoredChunk {
                chunk,
                vector,
                id: id.clone(),
            };
            self.chunks.insert(id, stored);
        }

        log::info!("Successfully added chunks. Total: {}", self.chunks.len());
        Ok(())
    }

    /// Search for similar chunks using semantic similarity
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        log::debug!("Searching for: '{}' (limit: {})", query, limit);

        // Embed query
        let query_vector = self.embedder.embed(query).await?;

        // Search HNSW index
        let neighbors = self.index.search(&query_vector, limit)?;

        // Convert to SearchResult
        let mut results = Vec::new();
        for (chunk_id, score) in neighbors {
            // Find chunk by numeric id
            if let Some(stored) = self.find_chunk_by_numeric_id(chunk_id) {
                results.push(SearchResult {
                    chunk: stored.chunk.clone(),
                    score,
                    id: stored.id.clone(),
                });
            }
        }

        log::debug!("Found {} results", results.len());
        Ok(results)
    }

    /// Find chunk by numeric ID using id_map
    fn find_chunk_by_numeric_id(&self, id: usize) -> Option<&StoredChunk> {
        self.id_map.get(&id).and_then(|string_id| self.chunks.get(string_id))
    }

    /// Get chunk by string ID
    pub fn get_chunk(&self, id: &str) -> Option<&StoredChunk> {
        self.chunks.get(id)
    }

    /// Get all chunk IDs
    pub fn chunk_ids(&self) -> Vec<String> {
        self.chunks.keys().cloned().collect()
    }

    /// Get total number of chunks
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// Check if store is empty
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Save store to disk
    pub async fn save(&self) -> Result<()> {
        log::info!("Saving VectorStore to {:?}", self.path);

        // Save both chunks and id_map
        let save_data = serde_json::json!({
            "chunks": self.chunks,
            "id_map": self.id_map,
            "next_id": self.next_id,
        });

        let data = serde_json::to_string_pretty(&save_data)?;
        tokio::fs::write(&self.path, data).await?;
        log::info!("VectorStore saved successfully");
        Ok(())
    }

    /// Load store from disk
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        log::info!("Loading VectorStore from {:?}", path.as_ref());
        let data = tokio::fs::read_to_string(&path).await?;
        let save_data: serde_json::Value = serde_json::from_str(&data)?;

        // Load chunks and id_map
        let chunks: HashMap<String, StoredChunk> =
            serde_json::from_value(save_data["chunks"].clone())?;
        let id_map: HashMap<usize, String> =
            serde_json::from_value(save_data["id_map"].clone())?;
        let next_id: usize = save_data["next_id"].as_u64().unwrap_or(0) as usize;

        let embedder = EmbeddingModel::new().await?;
        let dimension = embedder.dimension();
        let mut index = HnswIndex::new(dimension);

        // Rebuild index using id_map
        for (&numeric_id, string_id) in &id_map {
            if let Some(stored) = chunks.get(string_id) {
                index.add(numeric_id, &stored.vector)?;
            }
        }

        log::info!("Loaded {} chunks", chunks.len());

        Ok(Self {
            chunks,
            index,
            embedder,
            path: path.as_ref().to_path_buf(),
            next_id,
            id_map,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_code_chunker::{ChunkMetadata, CodeChunk};
    use tempfile::TempDir;

    fn create_test_chunk(path: &str, content: &str, line: usize) -> CodeChunk {
        CodeChunk::new(
            path.to_string(),
            line,
            line + 10,
            content.to_string(),
            ChunkMetadata::default(),
        )
    }

    #[tokio::test]
    #[ignore] // Requires model download
    async fn test_add_and_search() {
        let temp_dir = TempDir::new().unwrap();
        let store_path = temp_dir.path().join("store.json");
        let mut store = VectorStore::new(&store_path).await.unwrap();

        let chunks = vec![
            create_test_chunk("test.rs", "fn hello() { println!(\"hello\"); }", 1),
            create_test_chunk("test.rs", "fn goodbye() { println!(\"goodbye\"); }", 15),
        ];

        store.add_chunks(chunks).await.unwrap();
        assert_eq!(store.len(), 2);

        let results = store.search("greeting function", 5).await.unwrap();
        assert!(!results.is_empty());
    }
}
