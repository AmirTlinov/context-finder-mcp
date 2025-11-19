use serde::{Deserialize, Serialize};

/// Configuration for code chunking behavior
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkerConfig {
    /// Chunking strategy to use
    pub strategy: ChunkingStrategy,

    /// Overlap strategy for context preservation
    pub overlap: OverlapStrategy,

    /// Target chunk size in tokens (soft limit)
    pub target_chunk_tokens: usize,

    /// Maximum chunk size in tokens (hard limit)
    pub max_chunk_tokens: usize,

    /// Minimum chunk size in tokens (avoid too small chunks)
    pub min_chunk_tokens: usize,

    /// Include imports as context in chunks
    pub include_imports: bool,

    /// Include parent scope context (class for methods, module for functions)
    pub include_parent_context: bool,

    /// Include documentation/docstrings
    pub include_documentation: bool,

    /// Maximum number of imports to include per chunk
    pub max_imports_per_chunk: usize,

    /// Languages to support (empty = all supported languages)
    pub supported_languages: Vec<String>,
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        Self {
            strategy: ChunkingStrategy::Semantic,
            overlap: OverlapStrategy::Contextual,
            target_chunk_tokens: 512,
            max_chunk_tokens: 1024,
            min_chunk_tokens: 10,
            include_imports: true,
            include_parent_context: true,
            include_documentation: true,
            max_imports_per_chunk: 10,
            supported_languages: vec![],
        }
    }
}

impl ChunkerConfig {
    /// Create config optimized for embeddings (smaller, focused chunks)
    pub fn for_embeddings() -> Self {
        Self {
            target_chunk_tokens: 384,
            max_chunk_tokens: 512,
            include_parent_context: true,
            include_documentation: true,
            ..Default::default()
        }
    }

    /// Create config optimized for LLM context (larger, comprehensive chunks)
    pub fn for_llm_context() -> Self {
        Self {
            target_chunk_tokens: 1024,
            max_chunk_tokens: 2048,
            include_imports: true,
            include_parent_context: true,
            include_documentation: true,
            ..Default::default()
        }
    }

    /// Create config optimized for speed (simpler chunking)
    pub fn for_speed() -> Self {
        Self {
            strategy: ChunkingStrategy::LineCount,
            overlap: OverlapStrategy::None,
            include_imports: false,
            include_parent_context: false,
            include_documentation: false,
            ..Default::default()
        }
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<(), String> {
        if self.min_chunk_tokens > self.target_chunk_tokens {
            return Err(format!(
                "min_chunk_tokens ({}) cannot exceed target_chunk_tokens ({})",
                self.min_chunk_tokens, self.target_chunk_tokens
            ));
        }

        if self.target_chunk_tokens > self.max_chunk_tokens {
            return Err(format!(
                "target_chunk_tokens ({}) cannot exceed max_chunk_tokens ({})",
                self.target_chunk_tokens, self.max_chunk_tokens
            ));
        }

        if self.max_chunk_tokens == 0 {
            return Err("max_chunk_tokens must be > 0".to_string());
        }

        Ok(())
    }
}

/// Strategy for chunking code
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkingStrategy {
    /// Semantic chunking based on AST boundaries (functions, classes, etc.)
    /// Best for preserving code structure and meaning
    Semantic,

    /// Fixed line count chunking (simpler, faster)
    /// Good for quick processing when structure is less important
    LineCount,

    /// Token-based chunking with awareness of syntax
    /// Balances size control with semantic preservation
    TokenAware,

    /// Hierarchical chunking (parent context + focused element)
    /// Best for understanding relationships
    Hierarchical,
}

/// Strategy for overlapping chunks to preserve context
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverlapStrategy {
    /// No overlap between chunks
    None,

    /// Fixed number of lines overlap
    FixedLines(usize),

    /// Fixed number of tokens overlap
    FixedTokens(usize),

    /// Contextual overlap (include imports and parent scopes)
    /// Smart overlap that preserves semantic context
    Contextual,

    /// Sliding window with specified overlap percentage (0-100)
    SlidingWindow(u8),
}

impl Default for OverlapStrategy {
    fn default() -> Self {
        Self::Contextual
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_valid() {
        let config = ChunkerConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_preset_configs_valid() {
        assert!(ChunkerConfig::for_embeddings().validate().is_ok());
        assert!(ChunkerConfig::for_llm_context().validate().is_ok());
        assert!(ChunkerConfig::for_speed().validate().is_ok());
    }

    #[test]
    fn test_config_validation() {
        let mut config = ChunkerConfig::default();

        // Invalid: min > target
        config.min_chunk_tokens = 1000;
        config.target_chunk_tokens = 500;
        assert!(config.validate().is_err());

        // Invalid: target > max
        config.min_chunk_tokens = 50;
        config.target_chunk_tokens = 2000;
        config.max_chunk_tokens = 1000;
        assert!(config.validate().is_err());

        // Invalid: max = 0
        config.max_chunk_tokens = 0;
        assert!(config.validate().is_err());

        // Valid configuration
        config.min_chunk_tokens = 50;
        config.target_chunk_tokens = 512;
        config.max_chunk_tokens = 1024;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_chunking_strategies() {
        let strategies = [
            ChunkingStrategy::Semantic,
            ChunkingStrategy::LineCount,
            ChunkingStrategy::TokenAware,
            ChunkingStrategy::Hierarchical,
        ];

        for strategy in strategies {
            let config = ChunkerConfig {
                strategy,
                ..Default::default()
            };
            assert!(config.validate().is_ok());
        }
    }

    #[test]
    fn test_overlap_strategies() {
        let strategies = [
            OverlapStrategy::None,
            OverlapStrategy::FixedLines(5),
            OverlapStrategy::FixedTokens(50),
            OverlapStrategy::Contextual,
            OverlapStrategy::SlidingWindow(20),
        ];

        for overlap in strategies {
            let config = ChunkerConfig {
                overlap,
                ..Default::default()
            };
            assert!(config.validate().is_ok());
        }
    }
}
