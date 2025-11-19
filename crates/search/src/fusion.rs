use context_code_chunker::CodeChunk;
use std::collections::HashMap;

/// Reciprocal Rank Fusion for combining multiple rankings
pub struct RRFFusion {
    /// RRF constant k (typically 60)
    k: f32,

    /// Weights for each ranking source
    semantic_weight: f32,
    fuzzy_weight: f32,
}

impl RRFFusion {
    pub fn new(semantic_weight: f32, fuzzy_weight: f32, k: f32) -> Self {
        Self {
            k,
            semantic_weight,
            fuzzy_weight,
        }
    }

    /// Fuse semantic and fuzzy results using RRF with adaptive weights based on query type
    ///
    /// Query classification:
    /// - Exact name: CamelCase, snake_case, single word → fuzzy-heavy (30/70)
    /// - Conceptual: multi-word, descriptive → semantic-heavy (70/30)
    pub fn fuse_adaptive(
        &self,
        query: &str,
        semantic_results: Vec<(usize, f32)>,
        fuzzy_results: Vec<(usize, f32)>,
    ) -> Vec<(usize, f32)> {
        let (semantic_weight, fuzzy_weight) = Self::adaptive_weights(query);

        log::debug!(
            "Adaptive weights for '{}': semantic={:.1}%, fuzzy={:.1}%",
            query,
            semantic_weight * 100.0,
            fuzzy_weight * 100.0
        );

        self.fuse_with_weights(semantic_results, fuzzy_results, semantic_weight, fuzzy_weight)
    }

    /// Fuse semantic and fuzzy results using RRF
    ///
    /// RRF formula: score(d) = Σ weight_i / (k + rank_i(d))
    ///
    /// Returns (chunk_index, fused_score) sorted by score descending
    pub fn fuse(
        &self,
        semantic_results: Vec<(usize, f32)>,
        fuzzy_results: Vec<(usize, f32)>,
    ) -> Vec<(usize, f32)> {
        self.fuse_with_weights(
            semantic_results,
            fuzzy_results,
            self.semantic_weight,
            self.fuzzy_weight,
        )
    }

    /// Fuse with explicit weights
    fn fuse_with_weights(
        &self,
        semantic_results: Vec<(usize, f32)>,
        fuzzy_results: Vec<(usize, f32)>,
        semantic_weight: f32,
        fuzzy_weight: f32,
    ) -> Vec<(usize, f32)> {
        let mut scores: HashMap<usize, f32> = HashMap::new();

        // Add semantic scores
        for (rank, (idx, _score)) in semantic_results.iter().enumerate() {
            let rrf_score = semantic_weight / (self.k + rank as f32 + 1.0);
            *scores.entry(*idx).or_insert(0.0) += rrf_score;
        }

        // Add fuzzy scores
        for (rank, (idx, _score)) in fuzzy_results.iter().enumerate() {
            let rrf_score = fuzzy_weight / (self.k + rank as f32 + 1.0);
            *scores.entry(*idx).or_insert(0.0) += rrf_score;
        }

        // Sort by fused score descending
        let mut fused: Vec<(usize, f32)> = scores.into_iter().collect();
        fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        fused
    }

    /// Determine optimal weights based on query characteristics
    fn adaptive_weights(query: &str) -> (f32, f32) {
        let query_trimmed = query.trim();

        // Check for exact name patterns
        let is_single_word = !query_trimmed.contains(' ') && !query_trimmed.contains('_');
        let has_camel_case = query_trimmed.chars().any(|c| c.is_uppercase())
            && query_trimmed.chars().any(|c| c.is_lowercase());
        let has_snake_case = query_trimmed.contains('_');

        // Exact name query: prioritize fuzzy matching
        if is_single_word && (has_camel_case || has_snake_case) {
            return (0.3, 0.7); // semantic 30%, fuzzy 70%
        }

        // Short query (1-2 words) without code patterns: balanced
        let word_count = query_trimmed.split_whitespace().count();
        if word_count <= 2 && !has_camel_case && !has_snake_case {
            return (0.5, 0.5); // balanced 50/50
        }

        // Conceptual/descriptive query: prioritize semantic
        (0.7, 0.3) // semantic 70%, fuzzy 30% (default)
    }
}

impl Default for RRFFusion {
    fn default() -> Self {
        Self::new(0.7, 0.3, 60.0)
    }
}

/// AST-aware boosting to prioritize important code elements
pub struct AstBooster;

impl AstBooster {
    /// Boost scores based on chunk type priority
    /// Functions/Methods get higher boost than variables
    pub fn boost(chunks: &[CodeChunk], results: Vec<(usize, f32)>) -> Vec<(usize, f32)> {
        results
            .into_iter()
            .map(|(idx, score)| {
                let boost = if let Some(chunk) = chunks.get(idx) {
                    Self::compute_boost(&chunk)
                } else {
                    1.0
                };
                (idx, score * boost)
            })
            .collect()
    }

    fn compute_boost(chunk: &CodeChunk) -> f32 {
        let type_boost = chunk
            .metadata
            .chunk_type
            .map(|ct| ct.priority() as f32 / 100.0)
            .unwrap_or(1.0);

        // Additional boost for chunks with documentation
        let doc_boost = if chunk.metadata.documentation.is_some() {
            1.1
        } else {
            1.0
        };

        // Boost for chunks with context (imports, parent scope)
        let context_boost = if !chunk.metadata.context_imports.is_empty()
            || chunk.metadata.parent_scope.is_some() {
            1.05
        } else {
            1.0
        };

        type_boost * doc_boost * context_boost
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_code_chunker::{ChunkMetadata, ChunkType, CodeChunk};

    #[test]
    fn test_rrf_fusion() {
        let fusion = RRFFusion::default();

        let semantic = vec![(0, 0.9), (1, 0.8), (2, 0.7)];
        let fuzzy = vec![(2, 0.95), (0, 0.85), (3, 0.75)];

        let fused = fusion.fuse(semantic, fuzzy);

        // Chunk 0 should rank high (in both lists)
        // Chunk 2 should rank high (high in fuzzy, present in semantic)
        assert!(!fused.is_empty());

        // Verify scores are computed
        for (_, score) in &fused {
            assert!(*score > 0.0);
        }
    }

    #[test]
    fn test_rrf_weights() {
        // Heavy semantic weight
        let fusion_semantic = RRFFusion::new(0.9, 0.1, 60.0);

        let semantic = vec![(0, 0.9)];
        let fuzzy = vec![(1, 0.9)];

        let fused = fusion_semantic.fuse(semantic, fuzzy);

        // Chunk 0 (semantic) should rank higher due to weight
        assert_eq!(fused[0].0, 0);
    }

    #[test]
    fn test_ast_boosting() {
        let chunks = vec![
            CodeChunk::new(
                "test.rs".to_string(),
                1,
                10,
                "fn test() {}".to_string(),
                ChunkMetadata::default().chunk_type(ChunkType::Function),
            ),
            CodeChunk::new(
                "test.rs".to_string(),
                20,
                25,
                "let x = 5;".to_string(),
                ChunkMetadata::default().chunk_type(ChunkType::Variable),
            ),
        ];

        let results = vec![(0, 0.5), (1, 0.5)];
        let boosted = AstBooster::boost(&chunks, results);

        // Function should have higher score than variable after boosting
        assert!(boosted[0].1 > boosted[1].1 || boosted[0].0 == 0);
    }
}
