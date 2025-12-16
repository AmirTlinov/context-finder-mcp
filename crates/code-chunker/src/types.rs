use serde::{Deserialize, Serialize};

/// A semantic code chunk with metadata
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeChunk {
    /// Source file path
    pub file_path: String,

    /// Start line (1-indexed)
    pub start_line: usize,

    /// End line (1-indexed, inclusive)
    pub end_line: usize,

    /// The actual code content
    pub content: String,

    /// Rich metadata about this chunk
    pub metadata: ChunkMetadata,
}

impl CodeChunk {
    /// Create a new code chunk
    #[must_use]
    pub const fn new(
        file_path: String,
        start_line: usize,
        end_line: usize,
        content: String,
        metadata: ChunkMetadata,
    ) -> Self {
        Self {
            file_path,
            start_line,
            end_line,
            content,
            metadata,
        }
    }

    /// Get the number of lines in this chunk
    #[must_use]
    pub const fn line_count(&self) -> usize {
        self.end_line.saturating_sub(self.start_line) + 1
    }

    /// Get estimated token count
    #[must_use]
    pub const fn estimated_tokens(&self) -> usize {
        self.metadata.estimated_tokens
    }

    /// Check if chunk contains a specific line
    #[must_use]
    pub const fn contains_line(&self, line: usize) -> bool {
        line >= self.start_line && line <= self.end_line
    }
}

/// Metadata about a code chunk
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChunkMetadata {
    /// Programming language
    pub language: Option<String>,

    /// Chunk type (function, class, module, etc.)
    pub chunk_type: Option<ChunkType>,

    /// Symbol name (function name, class name, etc.)
    pub symbol_name: Option<String>,

    /// Contextual imports included in this chunk
    pub context_imports: Vec<String>,

    /// Parent scope (class name for methods, module for functions)
    pub parent_scope: Option<String>,

    /// Estimated token count (rough approximation)
    pub estimated_tokens: usize,

    /// Full qualified name (e.g., "module.Class.method")
    pub qualified_name: Option<String>,

    /// Documentation/docstring if available
    pub documentation: Option<String>,

    /// Tags for categorization (async, public, deprecated, etc.)
    #[serde(default)]
    pub tags: Vec<String>,

    /// Tier/bundle markers (e.g., file/document/test)
    #[serde(default)]
    pub bundle_tags: Vec<String>,

    /// Related relative paths (tests, configs, docs)
    #[serde(default)]
    pub related_paths: Vec<String>,
}

impl ChunkMetadata {
    /// Create metadata with language only
    pub fn with_language(language: impl Into<String>) -> Self {
        Self {
            language: Some(language.into()),
            ..Default::default()
        }
    }

    /// Builder: set chunk type
    #[must_use]
    pub const fn chunk_type(mut self, chunk_type: ChunkType) -> Self {
        self.chunk_type = Some(chunk_type);
        self
    }

    /// Builder: set symbol name
    #[must_use]
    pub fn symbol_name(mut self, name: impl Into<String>) -> Self {
        self.symbol_name = Some(name.into());
        self
    }

    /// Builder: set parent scope
    #[must_use]
    pub fn parent_scope(mut self, scope: impl Into<String>) -> Self {
        self.parent_scope = Some(scope.into());
        self
    }

    /// Builder: add import
    #[must_use]
    pub fn add_import(mut self, import: impl Into<String>) -> Self {
        self.context_imports.push(import.into());
        self
    }

    /// Builder: set estimated tokens
    #[must_use]
    pub const fn estimated_tokens(mut self, tokens: usize) -> Self {
        self.estimated_tokens = tokens;
        self
    }

    /// Builder: add bundle tag
    #[must_use]
    pub fn add_bundle_tag(mut self, tag: impl Into<String>) -> Self {
        self.bundle_tags.push(tag.into());
        self
    }

    /// Builder: add related path
    #[must_use]
    pub fn add_related_path(mut self, path: impl Into<String>) -> Self {
        self.related_paths.push(path.into());
        self
    }

    /// Estimate tokens from content (rough heuristic: ~0.75 tokens per word)
    #[must_use]
    pub fn estimate_tokens_from_content(content: &str) -> usize {
        let chars = content.len();
        // Rough estimate: 4 chars per token on average for code
        (chars / 4).max(1)
    }
}

/// Type of code chunk based on semantic meaning
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum ChunkType {
    /// Standalone function
    Function,
    /// Method inside a class
    Method,
    /// Class definition
    Class,
    /// Struct definition
    Struct,
    /// Enum definition
    Enum,
    /// Interface/Trait definition
    Interface,
    /// Module definition
    Module,
    /// Implementation block
    Impl,
    /// Type alias
    Type,
    /// Constant
    Const,
    /// Variable declaration
    Variable,
    /// Import/use statement
    Import,
    /// Documentation comment
    Comment,
    /// Generic code block
    Other,
}

impl ChunkType {
    /// Get priority for chunking (higher = more important to keep intact)
    #[must_use]
    pub const fn priority(self) -> u8 {
        match self {
            Self::Function | Self::Method => 100,
            Self::Class | Self::Struct => 90,
            Self::Enum | Self::Interface => 85,
            Self::Impl => 80,
            Self::Type => 70,
            Self::Module => 60,
            Self::Const | Self::Variable => 50,
            Self::Import => 40,
            Self::Comment => 20,
            Self::Other => 10,
        }
    }

    /// Check if this chunk type should include contextual imports
    #[must_use]
    pub const fn needs_context(self) -> bool {
        matches!(
            self,
            Self::Function | Self::Method | Self::Class | Self::Struct | Self::Impl
        )
    }

    /// Check if this is a declaration type (vs usage)
    #[must_use]
    pub const fn is_declaration(self) -> bool {
        !matches!(self, Self::Import | Self::Comment | Self::Other)
    }

    /// Get human-readable name
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Class => "class",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Interface => "interface",
            Self::Module => "module",
            Self::Impl => "impl",
            Self::Type => "type",
            Self::Const => "const",
            Self::Variable => "variable",
            Self::Import => "import",
            Self::Comment => "comment",
            Self::Other => "other",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_line_count() {
        let chunk = CodeChunk::new(
            "test.rs".to_string(),
            10,
            15,
            "code".to_string(),
            ChunkMetadata::default(),
        );
        assert_eq!(chunk.line_count(), 6);
    }

    #[test]
    fn test_chunk_contains_line() {
        let chunk = CodeChunk::new(
            "test.rs".to_string(),
            10,
            15,
            "code".to_string(),
            ChunkMetadata::default(),
        );
        assert!(chunk.contains_line(10));
        assert!(chunk.contains_line(12));
        assert!(chunk.contains_line(15));
        assert!(!chunk.contains_line(9));
        assert!(!chunk.contains_line(16));
    }

    #[test]
    fn test_chunk_type_priority() {
        assert!(ChunkType::Function.priority() > ChunkType::Variable.priority());
        assert!(ChunkType::Class.priority() > ChunkType::Import.priority());
        assert_eq!(ChunkType::Function.priority(), ChunkType::Method.priority());
    }

    #[test]
    fn test_chunk_type_needs_context() {
        assert!(ChunkType::Function.needs_context());
        assert!(ChunkType::Class.needs_context());
        assert!(ChunkType::Method.needs_context());
        assert!(!ChunkType::Import.needs_context());
        assert!(!ChunkType::Comment.needs_context());
    }

    #[test]
    fn test_metadata_builder() {
        let metadata = ChunkMetadata::with_language("rust")
            .chunk_type(ChunkType::Function)
            .symbol_name("test_func")
            .parent_scope("TestModule")
            .add_import("std::collections::HashMap")
            .estimated_tokens(100);

        assert_eq!(metadata.language.as_deref(), Some("rust"));
        assert_eq!(metadata.chunk_type, Some(ChunkType::Function));
        assert_eq!(metadata.symbol_name.as_deref(), Some("test_func"));
        assert_eq!(metadata.parent_scope.as_deref(), Some("TestModule"));
        assert_eq!(metadata.context_imports.len(), 1);
        assert_eq!(metadata.estimated_tokens, 100);
    }

    #[test]
    fn test_estimate_tokens() {
        let content = "fn main() { println!(\"Hello\"); }";
        let tokens = ChunkMetadata::estimate_tokens_from_content(content);
        assert!(tokens > 0);
        assert!(tokens < 100);
    }
}
