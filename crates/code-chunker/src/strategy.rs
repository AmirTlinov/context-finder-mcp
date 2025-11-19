use crate::config::{ChunkerConfig, ChunkingStrategy};
use crate::types::{ChunkMetadata, CodeChunk};

/// Execute chunking strategy on source code
pub struct StrategyExecutor {
    config: ChunkerConfig,
}

impl StrategyExecutor {
    pub fn new(config: ChunkerConfig) -> Self {
        Self { config }
    }

    /// Execute the configured strategy
    pub fn execute(
        &self,
        content: &str,
        file_path: &str,
        language: &str,
    ) -> Vec<CodeChunk> {
        match self.config.strategy {
            ChunkingStrategy::LineCount => self.chunk_by_lines(content, file_path, language),
            ChunkingStrategy::Semantic => {
                // Will be delegated to AST analyzer for supported languages
                // Fallback to line-based for unsupported languages
                self.chunk_by_lines(content, file_path, language)
            }
            ChunkingStrategy::TokenAware => self.chunk_by_tokens(content, file_path, language),
            ChunkingStrategy::Hierarchical => {
                self.chunk_hierarchical(content, file_path, language)
            }
        }
    }

    /// Simple line-based chunking
    fn chunk_by_lines(
        &self,
        content: &str,
        file_path: &str,
        language: &str,
    ) -> Vec<CodeChunk> {
        let lines: Vec<&str> = content.lines().collect();
        let target_lines = self.estimate_target_lines();
        let mut chunks = Vec::new();
        let mut start = 0;

        while start < lines.len() {
            let end = (start + target_lines).min(lines.len());
            let chunk_content = lines[start..end].join("\n");
            let estimated_tokens = ChunkMetadata::estimate_tokens_from_content(&chunk_content);

            let metadata = ChunkMetadata {
                language: Some(language.to_string()),
                estimated_tokens,
                ..Default::default()
            };

            chunks.push(CodeChunk::new(
                file_path.to_string(),
                start + 1,
                end,
                chunk_content,
                metadata,
            ));

            start = end;
        }

        chunks
    }

    /// Token-aware chunking
    fn chunk_by_tokens(
        &self,
        content: &str,
        file_path: &str,
        language: &str,
    ) -> Vec<CodeChunk> {
        let lines: Vec<&str> = content.lines().collect();
        let mut chunks = Vec::new();
        let mut start_line = 0;
        let mut current_tokens = 0;

        for (idx, line) in lines.iter().enumerate() {
            let line_tokens = ChunkMetadata::estimate_tokens_from_content(line);
            current_tokens += line_tokens;
            let current_end = idx + 1;

            if current_tokens >= self.config.target_chunk_tokens {
                let chunk_content = lines[start_line..current_end].join("\n");
                let metadata = ChunkMetadata {
                    language: Some(language.to_string()),
                    estimated_tokens: current_tokens,
                    ..Default::default()
                };

                chunks.push(CodeChunk::new(
                    file_path.to_string(),
                    start_line + 1,
                    current_end,
                    chunk_content,
                    metadata,
                ));

                start_line = current_end;
                current_tokens = 0;
            }
        }

        // Handle remaining lines
        if start_line < lines.len() {
            let chunk_content = lines[start_line..].join("\n");
            let estimated_tokens = ChunkMetadata::estimate_tokens_from_content(&chunk_content);

            let metadata = ChunkMetadata {
                language: Some(language.to_string()),
                estimated_tokens,
                ..Default::default()
            };

            chunks.push(CodeChunk::new(
                file_path.to_string(),
                start_line + 1,
                lines.len(),
                chunk_content,
                metadata,
            ));
        }

        chunks
    }

    /// Hierarchical chunking (simplified version)
    fn chunk_hierarchical(
        &self,
        content: &str,
        file_path: &str,
        language: &str,
    ) -> Vec<CodeChunk> {
        // For now, delegate to token-aware
        // Full implementation would require AST analysis
        self.chunk_by_tokens(content, file_path, language)
    }

    /// Estimate target lines from target tokens
    fn estimate_target_lines(&self) -> usize {
        // Rough estimate: ~10 tokens per line of code on average
        (self.config.target_chunk_tokens / 10).max(10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChunkerConfig;

    fn create_test_content() -> String {
        let mut lines = Vec::new();
        for i in 0..100 {
            lines.push(format!("fn test_function_{}() {{ }}", i));
        }
        lines.join("\n")
    }

    #[test]
    fn test_chunk_by_lines() {
        let config = ChunkerConfig::default();
        let executor = StrategyExecutor::new(config);
        let content = create_test_content();

        let chunks = executor.chunk_by_lines(&content, "test.rs", "rust");
        assert!(!chunks.is_empty());
        assert!(chunks.len() > 1);

        for chunk in &chunks {
            assert!(chunk.line_count() > 0);
            assert!(!chunk.content.is_empty());
        }
    }

    #[test]
    fn test_chunk_by_tokens() {
        let config = ChunkerConfig {
            target_chunk_tokens: 100,
            ..Default::default()
        };
        let max_tokens = config.max_chunk_tokens;
        let executor = StrategyExecutor::new(config);
        let content = create_test_content();

        let chunks = executor.chunk_by_tokens(&content, "test.rs", "rust");
        assert!(!chunks.is_empty());

        for chunk in &chunks {
            // Most chunks should be close to target (last chunk may be smaller)
            assert!(
                chunk.estimated_tokens() <= max_tokens,
                "Chunk exceeds max tokens"
            );
        }
    }

    #[test]
    fn test_execute_strategies() {
        let content = create_test_content();

        let strategies = [
            ChunkingStrategy::LineCount,
            ChunkingStrategy::TokenAware,
            ChunkingStrategy::Semantic,
            ChunkingStrategy::Hierarchical,
        ];

        for strategy in strategies {
            let config = ChunkerConfig {
                strategy,
                ..Default::default()
            };
            let executor = StrategyExecutor::new(config);
            let chunks = executor.execute(&content, "test.rs", "rust");
            assert!(!chunks.is_empty(), "Strategy {:?} produced no chunks", strategy);
        }
    }
}
