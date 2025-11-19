use crate::error::{Result, SearchError};
use crate::fusion::{AstBooster, RRFFusion};
use crate::fuzzy::FuzzySearch;
use context_code_chunker::CodeChunk;
use context_vector_store::{SearchResult, VectorStore};

/// Hybrid search combining semantic, fuzzy, and RRF fusion
pub struct HybridSearch {
    store: VectorStore,
    chunks: Vec<CodeChunk>,
    fuzzy: FuzzySearch,
    fusion: RRFFusion,
}

impl HybridSearch {
    /// Create new hybrid search engine
    pub async fn new(store: VectorStore, chunks: Vec<CodeChunk>) -> Result<Self> {
        Ok(Self {
            store,
            chunks,
            fuzzy: FuzzySearch::new(),
            fusion: RRFFusion::default(),
        })
    }

    /// Search with full hybrid strategy: semantic + fuzzy + RRF + AST boost
    pub async fn search(&mut self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        if query.trim().is_empty() {
            return Err(SearchError::EmptyQuery);
        }

        log::debug!("Hybrid search: query='{}', limit={}", query, limit);

        // Candidate pool size (retrieve more for fusion)
        let candidate_pool = limit * 5;

        // Build chunk id -> index mapping
        let mut chunk_id_to_idx: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for (idx, chunk) in self.chunks.iter().enumerate() {
            let id = format!("{}:{}:{}", chunk.file_path, chunk.start_line, chunk.end_line);
            chunk_id_to_idx.insert(id, idx);
        }

        // 1. Semantic search (embeddings + cosine similarity)
        let semantic_results = self.store.search(query, candidate_pool).await?;
        log::debug!("Semantic: {} results", semantic_results.len());

        // Convert semantic results to (chunk_idx, score) using chunk_id_to_idx
        let semantic_scores: Vec<(usize, f32)> = semantic_results
            .iter()
            .filter_map(|result| {
                chunk_id_to_idx
                    .get(&result.id)
                    .map(|&idx| (idx, result.score))
            })
            .collect();

        // 2. Fuzzy search (path/symbol matching)
        let fuzzy_scores = self.fuzzy.search(query, &self.chunks, candidate_pool);
        log::debug!("Fuzzy: {} results", fuzzy_scores.len());

        // 3. RRF Fusion
        let fused_scores = self.fusion.fuse(semantic_scores, fuzzy_scores);
        log::debug!("Fused: {} results", fused_scores.len());

        // 4. AST-aware boosting
        let boosted_scores = AstBooster::boost(&self.chunks, fused_scores);

        // 5. Convert back to SearchResult using chunk indices
        let mut final_results: Vec<SearchResult> = boosted_scores
            .into_iter()
            .filter_map(|(idx, score)| {
                self.chunks.get(idx).map(|chunk| {
                    let id = format!("{}:{}:{}", chunk.file_path, chunk.start_line, chunk.end_line);
                    SearchResult {
                        chunk: chunk.clone(),
                        score,
                        id,
                    }
                })
            })
            .collect();

        // Sort by final score descending
        final_results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        final_results.truncate(limit);

        log::info!("Hybrid search completed: {} final results", final_results.len());

        Ok(final_results)
    }

    /// Semantic-only search (bypass fuzzy/fusion for speed)
    pub async fn search_semantic_only(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        if query.trim().is_empty() {
            return Err(SearchError::EmptyQuery);
        }

        self.store.search(query, limit).await.map_err(Into::into)
    }

    /// Get chunk by ID
    pub fn get_chunk(&self, id: &str) -> Option<&CodeChunk> {
        self.chunks.iter().find(|c| {
            let chunk_id = format!("{}:{}:{}", c.file_path, c.start_line, c.end_line);
            chunk_id == id
        })
    }

    /// Get all chunks
    pub fn chunks(&self) -> &[CodeChunk] {
        &self.chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_code_chunker::{ChunkMetadata, ChunkType};
    use tempfile::TempDir;

    fn create_test_chunk(path: &str, line: usize, symbol: &str, content: &str) -> CodeChunk {
        CodeChunk::new(
            path.to_string(),
            line,
            line + 10,
            content.to_string(),
            ChunkMetadata::default()
                .chunk_type(ChunkType::Function)
                .symbol_name(symbol),
        )
    }

    #[tokio::test]
    #[ignore] // Requires FastEmbed model
    async fn test_hybrid_search() {
        let temp_dir = TempDir::new().unwrap();
        let store_path = temp_dir.path().join("store.json");

        let chunks = vec![
            create_test_chunk("api.rs", 1, "handle_error", "async fn handle_error() { /* error handling */ }"),
            create_test_chunk("utils.rs", 20, "parse_data", "fn parse_data(input: &str) -> Result<Data> {}"),
            create_test_chunk("main.rs", 50, "main", "fn main() { println!(\"hello\"); }"),
        ];

        let mut store = VectorStore::new(&store_path).await.unwrap();
        store.add_chunks(chunks.clone()).await.unwrap();

        let mut search = HybridSearch::new(store, chunks).await.unwrap();

        let results = search.search("error handling", 5).await.unwrap();
        assert!(!results.is_empty());
    }
}
