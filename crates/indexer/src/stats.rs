use serde::{Deserialize, Serialize};

/// Statistics about indexing operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStats {
    /// Number of files processed
    pub files: usize,

    /// Number of chunks created
    pub chunks: usize,

    /// Total lines of code
    pub total_lines: usize,

    /// Time taken in milliseconds
    pub time_ms: u64,

    /// Languages found
    pub languages: std::collections::HashMap<String, usize>,

    /// Errors encountered
    pub errors: Vec<String>,
}

impl IndexStats {
    pub fn new() -> Self {
        Self {
            files: 0,
            chunks: 0,
            total_lines: 0,
            time_ms: 0,
            languages: std::collections::HashMap::new(),
            errors: Vec::new(),
        }
    }

    pub fn add_file(&mut self, language: &str, lines: usize) {
        self.files += 1;
        self.total_lines += lines;
        *self.languages.entry(language.to_string()).or_insert(0) += 1;
    }

    pub fn add_chunks(&mut self, count: usize) {
        self.chunks += count;
    }

    pub fn add_error(&mut self, error: String) {
        self.errors.push(error);
    }
}

impl Default for IndexStats {
    fn default() -> Self {
        Self::new()
    }
}
