use thiserror::Error;

pub type Result<T> = std::result::Result<T, GraphError>;

#[derive(Error, Debug)]
pub enum GraphError {
    #[error("Graph build error: {0}")]
    BuildError(String),

    #[error("Node not found: {0}")]
    NodeNotFound(String),

    #[error("Invalid symbol: {0}")]
    InvalidSymbol(String),

    #[error("Traversal error: {0}")]
    TraversalError(String),

    #[error("{0}")]
    Other(String),
}
