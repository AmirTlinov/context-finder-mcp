use crate::ast_analyzer::AstAnalyzer;
use crate::config::ChunkerConfig;
use crate::error::{ChunkerError, Result};
use crate::language::Language;
use crate::strategy::StrategyExecutor;
use crate::types::CodeChunk;
use std::path::Path;

/// Main chunker interface for processing code
pub struct Chunker {
    config: ChunkerConfig,
}

impl Chunker {
    /// Create a new chunker with configuration
    pub fn new(config: ChunkerConfig) -> Self {
        config
            .validate()
            .expect("Invalid chunker configuration provided");
        Self { config }
    }

    /// Create chunker with default configuration
    pub fn default() -> Self {
        Self::new(ChunkerConfig::default())
    }

    /// Chunk code from a string
    pub fn chunk_str(&self, content: &str, file_path: Option<&str>) -> Result<Vec<CodeChunk>> {
        if content.is_empty() {
            return Err(ChunkerError::EmptyContent);
        }

        let file_path = file_path.unwrap_or("unknown");
        let language = Language::from_path(file_path);

        self.chunk_with_language(content, file_path, language)
    }

    /// Chunk code from a file
    pub fn chunk_file(&self, path: impl AsRef<Path>) -> Result<Vec<CodeChunk>> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)?;
        let file_path = path.to_str().unwrap_or("unknown");
        let language = Language::from_path(path);

        self.chunk_with_language(&content, file_path, language)
    }

    /// Chunk code with explicit language
    pub fn chunk_with_language(
        &self,
        content: &str,
        file_path: &str,
        language: Language,
    ) -> Result<Vec<CodeChunk>> {
        if content.is_empty() {
            return Err(ChunkerError::EmptyContent);
        }

        // Filter by supported languages if configured
        if !self.config.supported_languages.is_empty()
            && !self
                .config
                .supported_languages
                .contains(&language.as_str().to_string())
        {
            return Err(ChunkerError::unsupported_language(language.as_str()));
        }

        // Try AST-based chunking for supported languages
        if language.supports_ast() && self.config.strategy == crate::config::ChunkingStrategy::Semantic
        {
            match self.chunk_with_ast(content, file_path, language) {
                Ok(chunks) => return Ok(self.post_process_chunks(chunks)),
                Err(e) => {
                    log::warn!("AST chunking failed, falling back to strategy-based: {}", e);
                }
            }
        }

        // Fallback to strategy-based chunking
        let chunks = self.chunk_with_strategy(content, file_path, language);
        Ok(self.post_process_chunks(chunks))
    }

    /// Chunk using AST analysis
    fn chunk_with_ast(
        &self,
        content: &str,
        file_path: &str,
        language: Language,
    ) -> Result<Vec<CodeChunk>> {
        let mut analyzer = AstAnalyzer::new(self.config.clone(), language)?;
        analyzer.chunk(content, file_path)
    }

    /// Chunk using strategy executor
    fn chunk_with_strategy(
        &self,
        content: &str,
        file_path: &str,
        language: Language,
    ) -> Vec<CodeChunk> {
        let executor = StrategyExecutor::new(self.config.clone());
        executor.execute(content, file_path, language.as_str())
    }

    /// Post-process chunks (filtering, merging, etc.)
    fn post_process_chunks(&self, mut chunks: Vec<CodeChunk>) -> Vec<CodeChunk> {
        let min_tokens = self.config.min_chunk_tokens;
        // Filter out chunks that are too small
        chunks.retain(|chunk| {
            chunk.estimated_tokens() >= min_tokens
        });

        // TODO: Implement overlap strategy
        // TODO: Add contextual imports if configured
        // TODO: Merge small adjacent chunks if beneficial

        chunks
    }

    /// Get configuration
    pub fn config(&self) -> &ChunkerConfig {
        &self.config
    }

    /// Get statistics about chunking
    pub fn get_stats(chunks: &[CodeChunk]) -> ChunkingStats {
        ChunkingStats {
            total_chunks: chunks.len(),
            total_lines: chunks.iter().map(|c| c.line_count()).sum(),
            total_tokens: chunks.iter().map(|c| c.estimated_tokens()).sum(),
            avg_tokens_per_chunk: if chunks.is_empty() {
                0
            } else {
                chunks.iter().map(|c| c.estimated_tokens()).sum::<usize>() / chunks.len()
            },
            min_tokens: chunks
                .iter()
                .map(|c| c.estimated_tokens())
                .min()
                .unwrap_or(0),
            max_tokens: chunks
                .iter()
                .map(|c| c.estimated_tokens())
                .max()
                .unwrap_or(0),
        }
    }
}

/// Statistics about chunking results
#[derive(Debug, Clone)]
pub struct ChunkingStats {
    pub total_chunks: usize,
    pub total_lines: usize,
    pub total_tokens: usize,
    pub avg_tokens_per_chunk: usize,
    pub min_tokens: usize,
    pub max_tokens: usize,
}

impl std::fmt::Display for ChunkingStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Chunks: {} | Lines: {} | Tokens: {} | Avg: {} | Range: {}-{}",
            self.total_chunks,
            self.total_lines,
            self.total_tokens,
            self.avg_tokens_per_chunk,
            self.min_tokens,
            self.max_tokens
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_CODE: &str = r#"
use std::collections::HashMap;

/// Main function
fn main() {
    println!("Hello, world!");
}

struct Point {
    x: i32,
    y: i32,
}

impl Point {
    fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}
"#;

    #[test]
    fn test_chunk_str() {
        let chunker = Chunker::default();
        let chunks = chunker.chunk_str(RUST_CODE, Some("test.rs")).unwrap();
        assert!(!chunks.is_empty());
    }

    #[test]
    fn test_chunk_empty_content() {
        let chunker = Chunker::default();
        let result = chunker.chunk_str("", Some("test.rs"));
        assert!(result.is_err());
    }

    #[test]
    fn test_chunk_with_ast() {
        let config = ChunkerConfig {
            strategy: crate::config::ChunkingStrategy::Semantic,
            ..Default::default()
        };
        let chunker = Chunker::new(config);

        let chunks = chunker
            .chunk_with_language(RUST_CODE, "test.rs", Language::Rust)
            .unwrap();

        assert!(!chunks.is_empty());
        // Should have at least function, struct, impl
        assert!(chunks.len() >= 3);
    }

    #[test]
    fn test_chunking_stats() {
        let chunker = Chunker::default();
        let chunks = chunker.chunk_str(RUST_CODE, Some("test.rs")).unwrap();
        let stats = Chunker::get_stats(&chunks);

        assert_eq!(stats.total_chunks, chunks.len());
        assert!(stats.total_tokens > 0);
        assert!(stats.avg_tokens_per_chunk > 0);
    }

    #[test]
    fn test_different_strategies() {
        let strategies = [
            crate::config::ChunkingStrategy::LineCount,
            crate::config::ChunkingStrategy::TokenAware,
            crate::config::ChunkingStrategy::Semantic,
        ];

        for strategy in strategies {
            let chunker = Chunker::new(ChunkerConfig {
                strategy,
                ..Default::default()
            });

            let result = chunker.chunk_str(RUST_CODE, Some("test.rs"));
            assert!(
                result.is_ok(),
                "Strategy {:?} failed: {:?}",
                strategy,
                result.err()
            );
        }
    }
}
