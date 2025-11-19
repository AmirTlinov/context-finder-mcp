use thiserror::Error;

pub type Result<T> = std::result::Result<T, SearchError>;

#[derive(Error, Debug)]
pub enum SearchError {
    #[error("Vector store error: {0}")]
    VectorStoreError(#[from] context_vector_store::VectorStoreError),

    #[error("Graph error: {0}")]
    GraphError(#[from] context_graph::GraphError),

    #[error("Empty query")]
    EmptyQuery,

    #[error("{0}")]
    Other(String),
}
