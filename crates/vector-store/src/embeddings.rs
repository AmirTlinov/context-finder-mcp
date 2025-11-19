use crate::error::{Result, VectorStoreError};
use fastembed::TextEmbedding;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Embedding model for semantic search
pub struct EmbeddingModel {
    model: Arc<Mutex<TextEmbedding>>,
    dimension: usize,
}

impl EmbeddingModel {
    /// Create new embedding model (FastEmbed - all-MiniLM-L6-v2)
    pub async fn new() -> Result<Self> {
        log::info!("Initializing FastEmbed model...");

        let model = TextEmbedding::try_new(Default::default())
        .map_err(|e| {
            VectorStoreError::EmbeddingError(format!("Failed to initialize FastEmbed: {}", e))
        })?;

        let dimension = 384; // all-MiniLM-L6-v2 produces 384d vectors

        log::info!("FastEmbed model loaded (dimension: {})", dimension);

        Ok(Self {
            model: Arc::new(Mutex::new(model)),
            dimension,
        })
    }

    /// Get vector dimension
    pub fn dimension(&self) -> usize {
        self.dimension
    }

    /// Embed single text
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let embeddings = self.embed_batch(vec![text]).await?;
        embeddings
            .into_iter()
            .next()
            .ok_or_else(|| VectorStoreError::EmbeddingError("Empty embedding result".to_string()))
    }

    /// Embed batch of texts (much more efficient)
    pub async fn embed_batch(&self, texts: Vec<&str>) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let model = self.model.lock().await;

        let embeddings = model
            .embed(texts.to_vec(), None)
            .map_err(|e| VectorStoreError::EmbeddingError(format!("Embedding failed: {}", e)))?;

        Ok(embeddings)
    }

    /// Compute cosine similarity between two vectors
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() {
            return 0.0;
        }

        let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

        if norm_a == 0.0 || norm_b == 0.0 {
            return 0.0;
        }

        dot_product / (norm_a * norm_b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore] // Requires model download
    async fn test_embed_single() {
        let model = EmbeddingModel::new().await.unwrap();
        let embedding = model.embed("hello world").await.unwrap();
        assert_eq!(embedding.len(), 384);
    }

    #[tokio::test]
    #[ignore] // Requires model download
    async fn test_embed_batch() {
        let model = EmbeddingModel::new().await.unwrap();
        let texts = vec!["hello world", "foo bar", "test"];
        let embeddings = model.embed_batch(texts).await.unwrap();
        assert_eq!(embeddings.len(), 3);
        for emb in embeddings {
            assert_eq!(emb.len(), 384);
        }
    }

    #[test]
    fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let sim = EmbeddingModel::cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-6);

        let c = vec![1.0, 0.0];
        let d = vec![0.0, 1.0];
        let sim2 = EmbeddingModel::cosine_similarity(&c, &d);
        assert!((sim2 - 0.0).abs() < 1e-6);
    }
}
