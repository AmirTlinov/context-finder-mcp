use thiserror::Error;

/// Result type for chunker operations
pub type Result<T> = std::result::Result<T, ChunkerError>;

/// Errors that can occur during code chunking
#[derive(Error, Debug)]
pub enum ChunkerError {
    /// Failed to parse the source code
    #[error("Parse error: {0}")]
    ParseError(String),

    /// Unsupported language
    #[error("Unsupported language: {0}")]
    UnsupportedLanguage(String),

    /// Invalid configuration
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    /// IO error occurred
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    /// Invalid chunk boundaries
    #[error("Invalid chunk boundaries: start={start}, end={end}")]
    InvalidBoundaries { start: usize, end: usize },

    /// Empty content
    #[error("Empty content provided")]
    EmptyContent,

    /// Tree-sitter error
    #[error("Tree-sitter error: {0}")]
    TreeSitterError(String),

    /// Generic error
    #[error("{0}")]
    Other(String),
}

impl ChunkerError {
    /// Create a parse error
    pub fn parse(msg: impl Into<String>) -> Self {
        Self::ParseError(msg.into())
    }

    /// Create an unsupported language error
    pub fn unsupported_language(lang: impl Into<String>) -> Self {
        Self::UnsupportedLanguage(lang.into())
    }

    /// Create an invalid config error
    pub fn invalid_config(msg: impl Into<String>) -> Self {
        Self::InvalidConfig(msg.into())
    }

    /// Create a tree-sitter error
    pub fn tree_sitter(msg: impl Into<String>) -> Self {
        Self::TreeSitterError(msg.into())
    }
}
