use thiserror::Error;

pub type Result<T> = std::result::Result<T, VectorStoreError>;

#[derive(Error, Debug)]
pub enum VectorStoreError {
    #[error("Embedding error: {0}")]
    EmbeddingError(String),

    #[error("Index error: {0}")]
    IndexError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Invalid vector dimension: expected {expected}, got {actual}")]
    InvalidDimension { expected: usize, actual: usize },

    #[error("{0}")]
    Other(String),
}
