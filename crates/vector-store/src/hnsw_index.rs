use crate::embeddings::EmbeddingModel;
use crate::error::{Result, VectorStoreError};
use std::collections::HashMap;

/// Simple vector index (brute-force for now, can upgrade to HNSW later)
pub struct HnswIndex {
    dimension: usize,
    vectors: HashMap<usize, Vec<f32>>,
}

impl HnswIndex {
    pub fn new(dimension: usize) -> Self {
        Self {
            dimension,
            vectors: HashMap::new(),
        }
    }

    /// Add vector to index
    pub fn add(&mut self, id: usize, vector: &[f32]) -> Result<()> {
        if vector.len() != self.dimension {
            return Err(VectorStoreError::InvalidDimension {
                expected: self.dimension,
                actual: vector.len(),
            });
        }
        self.vectors.insert(id, vector.to_vec());
        Ok(())
    }

    /// Search for k nearest neighbors using cosine similarity
    /// Returns (id, score) sorted by score descending
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(usize, f32)>> {
        if query.len() != self.dimension {
            return Err(VectorStoreError::InvalidDimension {
                expected: self.dimension,
                actual: query.len(),
            });
        }

        // Brute-force search (O(n), but simple and correct)
        let mut scores: Vec<(usize, f32)> = self
            .vectors
            .iter()
            .map(|(id, vector)| {
                let similarity = EmbeddingModel::cosine_similarity(query, vector);
                (*id, similarity)
            })
            .collect();

        // Sort by score descending
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Take top k
        scores.truncate(k);

        Ok(scores)
    }

    /// Get number of vectors in index
    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    /// Check if index is empty
    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    /// Clear all vectors
    pub fn clear(&mut self) {
        self.vectors.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_search() {
        let mut index = HnswIndex::new(3);

        // Add some vectors
        index.add(0, &[1.0, 0.0, 0.0]).unwrap();
        index.add(1, &[0.9, 0.1, 0.0]).unwrap();
        index.add(2, &[0.0, 1.0, 0.0]).unwrap();

        assert_eq!(index.len(), 3);

        // Search for nearest to [1, 0, 0]
        let results = index.search(&[1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(results.len(), 2);

        // First result should be id=0 (exact match)
        assert_eq!(results[0].0, 0);
        assert!((results[0].1 - 1.0).abs() < 1e-6);

        // Second should be id=1 (close)
        assert_eq!(results[1].0, 1);
        assert!(results[1].1 > 0.9);
    }

    #[test]
    fn test_dimension_mismatch() {
        let mut index = HnswIndex::new(3);
        let result = index.add(0, &[1.0, 0.0]); // Wrong dimension
        assert!(result.is_err());

        index.add(0, &[1.0, 0.0, 0.0]).unwrap();
        let result = index.search(&[1.0, 0.0], 1); // Wrong query dimension
        assert!(result.is_err());
    }
}
