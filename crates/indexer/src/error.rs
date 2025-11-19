use thiserror::Error;

pub type Result<T> = std::result::Result<T, IndexerError>;

#[derive(Error, Debug)]
pub enum IndexerError {
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Chunker error: {0}")]
    ChunkerError(#[from] context_code_chunker::ChunkerError),

    #[error("Vector store error: {0}")]
    VectorStoreError(#[from] context_vector_store::VectorStoreError),

    #[error("Invalid project path: {0}")]
    InvalidPath(String),

    #[error("{0}")]
    Other(String),
}
