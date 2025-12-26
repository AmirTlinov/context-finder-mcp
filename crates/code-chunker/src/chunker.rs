use crate::ast_analyzer::AstAnalyzer;
use crate::config::{ChunkerConfig, OverlapStrategy};
use crate::contextual_imports;
use crate::error::{ChunkerError, Result};
use crate::language::Language;
use crate::strategy::StrategyExecutor;
use crate::types::{ChunkMetadata, CodeChunk};
use std::path::Path;

/// Main chunker interface for processing code
pub struct Chunker {
    config: ChunkerConfig,
}

impl Chunker {
    /// Create a new chunker with configuration
    #[must_use]
    pub fn new(config: ChunkerConfig) -> Self {
        config
            .validate()
            .expect("Invalid chunker configuration provided");
        Self { config }
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
        if language.supports_ast()
            && self.config.strategy == crate::config::ChunkingStrategy::Semantic
        {
            match self.chunk_with_ast(content, file_path, language) {
                Ok(chunks) => return Ok(self.post_process_chunks(chunks)),
                Err(e) => {
                    log::warn!("AST chunking failed, falling back to strategy-based: {e}");
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
        chunks.sort_by(|a, b| {
            a.file_path
                .cmp(&b.file_path)
                .then_with(|| a.start_line.cmp(&b.start_line))
                .then_with(|| a.end_line.cmp(&b.end_line))
        });

        for chunk in &mut chunks {
            self.normalize_chunk_metadata(chunk);
        }

        if self.config.include_imports {
            self.infer_missing_imports(&mut chunks);
        }

        chunks = self.merge_small_adjacent_chunks(chunks);
        chunks = Self::drop_shadowed_untyped_chunks(chunks);
        chunks = self.apply_overlap(chunks);

        let min_tokens = self.config.min_chunk_tokens;
        if min_tokens > 0 {
            chunks.retain(|chunk| chunk.estimated_tokens() >= min_tokens);
        }

        chunks
    }

    fn normalize_chunk_metadata(&self, chunk: &mut CodeChunk) {
        if self.config.include_imports {
            chunk.metadata.context_imports.sort();
            chunk.metadata.context_imports.dedup();
            chunk
                .metadata
                .context_imports
                .truncate(self.config.max_imports_per_chunk);
        } else {
            chunk.metadata.context_imports.clear();
        }

        if !self.config.include_parent_context {
            chunk.metadata.parent_scope = None;
            chunk.metadata.qualified_name = chunk.metadata.symbol_name.clone();
        }

        if !self.config.include_documentation {
            chunk.metadata.documentation = None;
        }

        chunk.metadata.estimated_tokens = self.estimate_chunk_tokens(chunk);
    }

    fn estimate_chunk_tokens(&self, chunk: &CodeChunk) -> usize {
        let mut tokens = ChunkMetadata::estimate_tokens_from_content(&chunk.content);

        if self.config.include_parent_context {
            if let Some(scope) = &chunk.metadata.parent_scope {
                tokens = tokens.saturating_add(ChunkMetadata::estimate_tokens_from_content(scope));
            }
        }

        if self.config.include_documentation {
            if let Some(doc) = &chunk.metadata.documentation {
                tokens = tokens.saturating_add(ChunkMetadata::estimate_tokens_from_content(doc));
            }
        }

        if self.config.include_imports && !chunk.metadata.context_imports.is_empty() {
            let imports_tokens: usize = chunk
                .metadata
                .context_imports
                .iter()
                .map(|imp| ChunkMetadata::estimate_tokens_from_content(imp))
                .sum();
            tokens = tokens.saturating_add(imports_tokens);
        }

        tokens.max(1)
    }

    fn infer_missing_imports(&self, chunks: &mut [CodeChunk]) {
        let per_chunk_limit = self.config.max_imports_per_chunk;
        if per_chunk_limit == 0 {
            return;
        }

        let mut start = 0;
        while start < chunks.len() {
            let file_path = chunks[start].file_path.clone();
            let mut end = start + 1;
            while end < chunks.len() && chunks[end].file_path == file_path {
                end += 1;
            }

            let language = Language::from_path(&file_path);

            // Scan the prefix of the file for import statements. For non-AST strategies we may
            // not have global file context, so we reconstruct a bounded prefix from existing chunks.
            let mut prefix_lines: Vec<&str> = Vec::new();
            for chunk in &chunks[start..end] {
                for line in chunk.content.lines() {
                    if prefix_lines.len() >= 300 {
                        break;
                    }
                    prefix_lines.push(line);
                }
                if prefix_lines.len() >= 300 {
                    break;
                }
            }

            let file_imports =
                contextual_imports::extract_imports_from_lines(language, &prefix_lines, 20);
            if !file_imports.is_empty() {
                for chunk in &mut chunks[start..end] {
                    let remaining =
                        per_chunk_limit.saturating_sub(chunk.metadata.context_imports.len());
                    if remaining == 0 {
                        continue;
                    }
                    let relevant = contextual_imports::filter_relevant_imports(
                        language,
                        &file_imports,
                        &chunk.content,
                        remaining,
                    );
                    if !relevant.is_empty() {
                        chunk.metadata.context_imports.extend(relevant);
                        self.normalize_chunk_metadata(chunk);
                    }
                }
            }

            start = end;
        }
    }

    fn drop_shadowed_untyped_chunks(chunks: Vec<CodeChunk>) -> Vec<CodeChunk> {
        let mut out = Vec::with_capacity(chunks.len());
        let mut buffer: Vec<CodeChunk> = Vec::new();
        let mut current_file: Option<String> = None;

        for chunk in chunks {
            let is_same_file = current_file
                .as_deref()
                .is_some_and(|file| file == chunk.file_path.as_str());
            if !is_same_file && !buffer.is_empty() {
                out.extend(Self::filter_shadowed_untyped_in_file(std::mem::take(
                    &mut buffer,
                )));
            }

            if !is_same_file {
                current_file = Some(chunk.file_path.clone());
            }
            buffer.push(chunk);
        }

        if !buffer.is_empty() {
            out.extend(Self::filter_shadowed_untyped_in_file(buffer));
        }

        out
    }

    fn filter_shadowed_untyped_in_file(chunks: Vec<CodeChunk>) -> Vec<CodeChunk> {
        let typed_ranges: Vec<(usize, usize)> = chunks
            .iter()
            .filter(|chunk| chunk.metadata.chunk_type.is_some())
            .map(|chunk| (chunk.start_line, chunk.end_line))
            .collect();

        if typed_ranges.is_empty() {
            return chunks;
        }

        let mut out = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            if chunk.metadata.chunk_type.is_none()
                && Self::is_shadowed_by_typed(&typed_ranges, chunk.start_line, chunk.end_line)
            {
                continue;
            }
            out.push(chunk);
        }

        out
    }

    fn is_shadowed_by_typed(typed_ranges: &[(usize, usize)], start: usize, end: usize) -> bool {
        let len = end.saturating_sub(start).saturating_add(1);
        if len == 0 {
            return false;
        }

        for (typed_start, typed_end) in typed_ranges {
            let overlap_start = start.max(*typed_start);
            let overlap_end = end.min(*typed_end);
            if overlap_end < overlap_start {
                continue;
            }

            let overlap = overlap_end - overlap_start + 1;
            if overlap.saturating_mul(10) >= len.saturating_mul(9) {
                return true;
            }
        }

        false
    }

    fn merge_small_adjacent_chunks(&self, chunks: Vec<CodeChunk>) -> Vec<CodeChunk> {
        let max_tokens = self.config.max_chunk_tokens;
        let soft_threshold = self.config.target_chunk_tokens / 2;

        let mut out: Vec<CodeChunk> = Vec::new();

        for chunk in chunks {
            if let Some(prev) = out.last_mut() {
                let mergeable =
                    prev.metadata.chunk_type.is_none() && chunk.metadata.chunk_type.is_none();
                let same_file = prev.file_path == chunk.file_path;
                let contiguous = chunk.start_line <= prev.end_line.saturating_add(1);
                let combined_tokens = prev
                    .estimated_tokens()
                    .saturating_add(chunk.estimated_tokens());
                let small_enough = prev.estimated_tokens() < soft_threshold
                    || chunk.estimated_tokens() < soft_threshold;

                if mergeable
                    && same_file
                    && contiguous
                    && combined_tokens <= max_tokens
                    && small_enough
                {
                    if !prev.content.ends_with('\n') {
                        prev.content.push('\n');
                    }
                    prev.content.push_str(&chunk.content);

                    prev.end_line = prev.end_line.max(chunk.end_line);

                    // Merge metadata collections (keep ordering deterministic).
                    prev.metadata
                        .context_imports
                        .extend(chunk.metadata.context_imports);
                    prev.metadata.context_imports.sort();
                    prev.metadata.context_imports.dedup();

                    prev.metadata.tags.extend(chunk.metadata.tags);
                    prev.metadata.tags.sort();
                    prev.metadata.tags.dedup();

                    prev.metadata.bundle_tags.extend(chunk.metadata.bundle_tags);
                    prev.metadata.bundle_tags.sort();
                    prev.metadata.bundle_tags.dedup();

                    prev.metadata
                        .related_paths
                        .extend(chunk.metadata.related_paths);
                    prev.metadata.related_paths.sort();
                    prev.metadata.related_paths.dedup();

                    // Degrade scalar metadata when it no longer represents a single symbol.
                    if prev.metadata.language != chunk.metadata.language {
                        prev.metadata.language = None;
                    }
                    if prev.metadata.chunk_type != chunk.metadata.chunk_type {
                        prev.metadata.chunk_type = None;
                    }
                    if prev.metadata.symbol_name != chunk.metadata.symbol_name {
                        prev.metadata.symbol_name = None;
                    }
                    if prev.metadata.qualified_name != chunk.metadata.qualified_name {
                        prev.metadata.qualified_name = None;
                    }
                    if prev.metadata.parent_scope != chunk.metadata.parent_scope {
                        prev.metadata.parent_scope = None;
                    }
                    if prev.metadata.documentation != chunk.metadata.documentation {
                        prev.metadata.documentation = None;
                    }

                    self.normalize_chunk_metadata(prev);
                    continue;
                }
            }

            out.push(chunk);
        }

        out
    }

    fn apply_overlap(&self, mut chunks: Vec<CodeChunk>) -> Vec<CodeChunk> {
        let max_tokens = self.config.max_chunk_tokens;

        let overlap = match self.config.overlap {
            OverlapStrategy::None | OverlapStrategy::Contextual => return chunks,
            OverlapStrategy::FixedLines(lines) => OverlapSpec::FixedLines(lines),
            OverlapStrategy::FixedTokens(tokens) => OverlapSpec::FixedTokens(tokens),
            OverlapStrategy::SlidingWindow(pct) => OverlapSpec::SlidingWindow(pct),
        };

        let mut prev_idx: Option<usize> = None;
        for idx in 0..chunks.len() {
            let Some(prev_i) = prev_idx else {
                prev_idx = Some(idx);
                continue;
            };

            if chunks[idx].file_path != chunks[prev_i].file_path {
                prev_idx = Some(idx);
                continue;
            }

            let prev_content = chunks[prev_i].content.clone();
            let prev_lines: Vec<&str> = prev_content.lines().collect();
            if prev_lines.is_empty() {
                prev_idx = Some(idx);
                continue;
            }

            let mut selected: Vec<&str> = match overlap {
                OverlapSpec::FixedLines(n) => {
                    let n = n.min(prev_lines.len());
                    prev_lines[prev_lines.len() - n..].to_vec()
                }
                OverlapSpec::FixedTokens(tokens) => {
                    select_tail_lines_by_tokens(&prev_lines, tokens)
                }
                OverlapSpec::SlidingWindow(pct) => {
                    let pct = pct.min(100) as usize;
                    let prev_tokens = ChunkMetadata::estimate_tokens_from_content(&prev_content);
                    let tokens = prev_tokens.saturating_mul(pct) / 100;
                    select_tail_lines_by_tokens(&prev_lines, tokens)
                }
            };

            // Ensure overlap does not push the chunk over the hard token limit.
            while !selected.is_empty() {
                let overlap_text = format!("{}\n", selected.join("\n"));
                let candidate_content = format!("{overlap_text}{}", chunks[idx].content);

                let mut candidate = chunks[idx].clone();
                candidate.content = candidate_content;
                candidate.start_line = candidate.start_line.saturating_sub(selected.len()).max(1);
                candidate.metadata.estimated_tokens = self.estimate_chunk_tokens(&candidate);

                if candidate.estimated_tokens() <= max_tokens {
                    chunks[idx] = candidate;
                    break;
                }

                // Drop the oldest overlapped line first (keep the most recent context).
                selected.remove(0);
            }

            prev_idx = Some(idx);
        }

        chunks
    }

    /// Get configuration
    #[must_use]
    pub const fn config(&self) -> &ChunkerConfig {
        &self.config
    }

    /// Get statistics about chunking
    #[must_use]
    pub fn get_stats(chunks: &[CodeChunk]) -> ChunkingStats {
        ChunkingStats {
            total_chunks: chunks.len(),
            total_lines: chunks.iter().map(CodeChunk::line_count).sum(),
            total_tokens: chunks.iter().map(CodeChunk::estimated_tokens).sum(),
            avg_tokens_per_chunk: if chunks.is_empty() {
                0
            } else {
                chunks
                    .iter()
                    .map(CodeChunk::estimated_tokens)
                    .sum::<usize>()
                    / chunks.len()
            },
            min_tokens: chunks
                .iter()
                .map(CodeChunk::estimated_tokens)
                .min()
                .unwrap_or(0),
            max_tokens: chunks
                .iter()
                .map(CodeChunk::estimated_tokens)
                .max()
                .unwrap_or(0),
        }
    }
}

impl Default for Chunker {
    fn default() -> Self {
        Self::new(ChunkerConfig::default())
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
    use crate::types::ChunkType;

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

    #[test]
    fn post_process_applies_fixed_lines_overlap_without_exceeding_bounds() {
        let config = ChunkerConfig {
            min_chunk_tokens: 0,
            target_chunk_tokens: 1,
            max_chunk_tokens: 10_000,
            overlap: OverlapStrategy::FixedLines(1),
            ..Default::default()
        };
        let chunker = Chunker::new(config);

        let mk_chunk = |start: usize, end: usize, content: &str| {
            CodeChunk::new(
                "test.rs".to_string(),
                start,
                end,
                content.to_string(),
                ChunkMetadata::default()
                    .estimated_tokens(ChunkMetadata::estimate_tokens_from_content(content)),
            )
        };

        let chunks = vec![mk_chunk(1, 2, "a\nb"), mk_chunk(3, 4, "c\nd")];
        let out = chunker.post_process_chunks(chunks);

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].start_line, 1);
        assert_eq!(out[0].end_line, 2);
        assert_eq!(out[1].start_line, 2);
        assert_eq!(out[1].end_line, 4);
        assert_eq!(out[1].content, "b\nc\nd");
    }

    #[test]
    fn post_process_merges_small_adjacent_chunks() {
        let config = ChunkerConfig {
            min_chunk_tokens: 0,
            target_chunk_tokens: 100,
            max_chunk_tokens: 1_000,
            overlap: OverlapStrategy::None,
            ..Default::default()
        };
        let chunker = Chunker::new(config);

        let mk_chunk = |start: usize, end: usize, content: &str, symbol: &str| {
            let mut meta = ChunkMetadata::default()
                .estimated_tokens(ChunkMetadata::estimate_tokens_from_content(content));
            meta.symbol_name = Some(symbol.to_string());
            CodeChunk::new("test.rs".to_string(), start, end, content.to_string(), meta)
        };

        let chunks = vec![mk_chunk(1, 1, "a", "a"), mk_chunk(2, 2, "b", "b")];
        let out = chunker.post_process_chunks(chunks);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_line, 1);
        assert_eq!(out[0].end_line, 2);
        assert_eq!(out[0].content, "a\nb");
        assert!(out[0].metadata.symbol_name.is_none());
    }

    #[test]
    fn post_process_merges_before_min_tokens_filter() {
        let config = ChunkerConfig {
            min_chunk_tokens: 10,
            target_chunk_tokens: 20,
            max_chunk_tokens: 1_000,
            overlap: OverlapStrategy::None,
            include_imports: false,
            include_parent_context: false,
            include_documentation: false,
            max_imports_per_chunk: 0,
            supported_languages: Vec::new(),
            strategy: crate::config::ChunkingStrategy::LineCount,
        };
        let chunker = Chunker::new(config);

        let content_a = "a".repeat(36);
        let content_b = "b".repeat(36);

        let mk_chunk = |start: usize, end: usize, content: &str| {
            CodeChunk::new(
                "test.rs".to_string(),
                start,
                end,
                content.to_string(),
                ChunkMetadata::default()
                    .estimated_tokens(ChunkMetadata::estimate_tokens_from_content(content)),
            )
        };

        let chunks = vec![mk_chunk(1, 1, &content_a), mk_chunk(2, 2, &content_b)];
        let out = chunker.post_process_chunks(chunks);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_line, 1);
        assert_eq!(out[0].end_line, 2);
        assert!(out[0].estimated_tokens() >= 10);
        assert!(out[0].content.contains(&content_a));
        assert!(out[0].content.contains(&content_b));
    }

    #[test]
    fn post_process_infers_imports_before_filtering_small_chunks() {
        let config = ChunkerConfig {
            min_chunk_tokens: 10,
            target_chunk_tokens: 50,
            max_chunk_tokens: 1_000,
            overlap: OverlapStrategy::Contextual,
            include_imports: true,
            include_parent_context: false,
            include_documentation: false,
            max_imports_per_chunk: 10,
            supported_languages: Vec::new(),
            strategy: crate::config::ChunkingStrategy::LineCount,
        };
        let chunker = Chunker::new(config);

        let import_content = "use std::collections::HashMap;";
        let import_chunk = CodeChunk::new(
            "test.rs".to_string(),
            1,
            1,
            import_content.to_string(),
            ChunkMetadata::default()
                .estimated_tokens(ChunkMetadata::estimate_tokens_from_content(import_content)),
        );
        let func_content = "pub fn foo() -> HashMap<i32, i32> { HashMap::new() }";
        let func_chunk = CodeChunk::new(
            "test.rs".to_string(),
            2,
            4,
            func_content.to_string(),
            ChunkMetadata::default()
                .estimated_tokens(ChunkMetadata::estimate_tokens_from_content(func_content)),
        );

        let out = chunker.post_process_chunks(vec![import_chunk, func_chunk]);

        assert_eq!(out.len(), 1);
        let chunk = &out[0];
        assert!(chunk.content.contains("pub fn foo"));
        assert!(chunk
            .metadata
            .context_imports
            .iter()
            .any(|imp| imp.contains("std::collections::HashMap")));
    }

    #[test]
    fn post_process_drops_untyped_chunks_shadowed_by_typed_ranges() {
        let config = ChunkerConfig {
            min_chunk_tokens: 0,
            target_chunk_tokens: 100,
            max_chunk_tokens: 1_000,
            overlap: OverlapStrategy::Contextual,
            include_imports: false,
            include_parent_context: false,
            include_documentation: false,
            max_imports_per_chunk: 0,
            supported_languages: Vec::new(),
            strategy: crate::config::ChunkingStrategy::Semantic,
        };
        let chunker = Chunker::new(config);

        let typed = CodeChunk::new(
            "test.rs".to_string(),
            10,
            20,
            "fn foo() {}\n".to_string(),
            ChunkMetadata {
                chunk_type: Some(ChunkType::Function),
                symbol_name: Some("foo".to_string()),
                estimated_tokens: 50,
                ..Default::default()
            },
        );

        let untyped = CodeChunk::new(
            "test.rs".to_string(),
            10,
            20,
            "fn foo() {}\n".to_string(),
            ChunkMetadata {
                estimated_tokens: 50,
                ..Default::default()
            },
        );

        let out = chunker.post_process_chunks(vec![untyped, typed]);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].metadata.chunk_type, Some(ChunkType::Function));
        assert_eq!(out[0].metadata.symbol_name.as_deref(), Some("foo"));
    }
}

#[derive(Clone, Copy)]
enum OverlapSpec {
    FixedLines(usize),
    FixedTokens(usize),
    SlidingWindow(u8),
}

fn select_tail_lines_by_tokens<'a>(lines: &[&'a str], tokens: usize) -> Vec<&'a str> {
    if tokens == 0 || lines.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut remaining = tokens;

    for &line in lines.iter().rev() {
        let t = ChunkMetadata::estimate_tokens_from_content(line);
        out.push(line);
        if t >= remaining {
            break;
        }
        remaining -= t;
    }

    out.reverse();
    out
}
