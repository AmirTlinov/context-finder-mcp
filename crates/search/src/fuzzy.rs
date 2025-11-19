use context_code_chunker::CodeChunk;
use nucleo_matcher::{pattern::Pattern, Matcher};

/// Fuzzy search for code chunks using nucleo-matcher
pub struct FuzzySearch {
    matcher: Matcher,
}

impl FuzzySearch {
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(nucleo_matcher::Config::DEFAULT),
        }
    }

    /// Search chunks by fuzzy matching against paths and symbol names
    /// Returns (chunk_index, score) sorted by score descending
    pub fn search(&mut self, query: &str, chunks: &[CodeChunk], limit: usize) -> Vec<(usize, f32)> {
        let pattern = Pattern::parse(
            query,
            nucleo_matcher::pattern::CaseMatching::Smart,
            nucleo_matcher::pattern::Normalization::Smart,
        );

        let mut scored: Vec<(usize, u32)> = chunks
            .iter()
            .enumerate()
            .filter_map(|(idx, chunk)| {
                // Try matching against multiple targets
                let path_haystack = nucleo_matcher::Utf32String::from(chunk.file_path.as_str());
                let path_score = pattern.score(path_haystack.slice(..), &mut self.matcher);

                let symbol_score = chunk
                    .metadata
                    .symbol_name
                    .as_ref()
                    .and_then(|name| {
                        let symbol_haystack = nucleo_matcher::Utf32String::from(name.as_str());
                        pattern.score(symbol_haystack.slice(..), &mut self.matcher)
                    });

                let content_preview = if chunk.content.len() > 200 {
                    &chunk.content[..200]
                } else {
                    &chunk.content
                };
                let content_haystack = nucleo_matcher::Utf32String::from(content_preview);
                let content_score = pattern.score(content_haystack.slice(..), &mut self.matcher);

                // Take best score
                let best_score = [path_score, symbol_score, content_score]
                    .into_iter()
                    .flatten()
                    .max()?;

                Some((idx, best_score))
            })
            .collect();

        // Sort by score descending
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        scored.truncate(limit);

        // Normalize scores to 0-1 range (nucleo scores are u32)
        let max_score = scored.first().map(|(_, s)| *s as f32).unwrap_or(1.0);

        scored
            .into_iter()
            .map(|(idx, score)| {
                let normalized = if max_score > 0.0 {
                    score as f32 / max_score
                } else {
                    0.0
                };
                (idx, normalized)
            })
            .collect()
    }
}

impl Default for FuzzySearch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_code_chunker::{ChunkMetadata, ChunkType};

    fn create_chunk(path: &str, symbol: &str, content: &str) -> CodeChunk {
        CodeChunk::new(
            path.to_string(),
            1,
            10,
            content.to_string(),
            ChunkMetadata::default()
                .chunk_type(ChunkType::Function)
                .symbol_name(symbol),
        )
    }

    #[test]
    fn test_fuzzy_path_match() {
        let mut fuzzy = FuzzySearch::new();
        let chunks = vec![
            create_chunk("src/api/handler.rs", "process", "fn process() {}"),
            create_chunk("src/main.rs", "main", "fn main() {}"),
            create_chunk("tests/api_test.rs", "test", "fn test() {}"),
        ];

        let results = fuzzy.search("api", &chunks, 5);

        assert!(!results.is_empty());
        // "src/api/handler.rs" and "tests/api_test.rs" should match
        assert!(results.iter().any(|(idx, _)| *idx == 0));
    }

    #[test]
    fn test_fuzzy_symbol_match() {
        let mut fuzzy = FuzzySearch::new();
        let chunks = vec![
            create_chunk("test.rs", "get_user", "fn get_user() {}"),
            create_chunk("test.rs", "set_data", "fn set_data() {}"),
            create_chunk("test.rs", "fetch_item", "fn fetch_item() {}"),
        ];

        let results = fuzzy.search("get", &chunks, 5);

        assert!(!results.is_empty());
        // "get_user" should be first
        assert_eq!(results[0].0, 0);
    }

    #[test]
    fn test_fuzzy_typo_tolerance() {
        let mut fuzzy = FuzzySearch::new();
        let chunks = vec![
            create_chunk("test.rs", "process_data", "fn process_data() {}"),
        ];

        // "proces" (typo) should still match "process_data"
        let results = fuzzy.search("proces", &chunks, 5);
        assert!(!results.is_empty());
    }
}
