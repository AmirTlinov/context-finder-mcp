//! # Context Code Chunker
//!
//! Intelligent, AST-aware code chunking for semantic search and AI context.
//!
//! ## Philosophy
//!
//! The chunker creates semantically meaningful code fragments that:
//! - Preserve syntactic boundaries (functions, classes, modules)
//! - Include necessary context (imports, type definitions, parent scopes)
//! - Optimize for embedding quality and AI comprehension
//! - Support cross-chunk references via smart overlap
//!
//! ## Architecture
//!
//! ```text
//! Source Code
//!     │
//!     ├──> Language Detection (from extension/content)
//!     │
//!     ├──> Tree-sitter Parsing → AST
//!     │
//!     ├──> Semantic Analysis
//!     │    ├─> Find top-level declarations
//!     │    ├─> Extract context (imports, parent scopes)
//!     │    └─> Compute optimal chunk boundaries
//!     │
//!     └──> Chunk Generation
//!          ├─> Add contextual headers
//!          ├─> Apply overlap strategy
//!          └─> Emit CodeChunk[] with metadata
//! ```
//!
//! ## Example
//!
//! ```rust
//! use context_code_chunker::{Chunker, ChunkerConfig};
//!
//! let config = ChunkerConfig::default();
//! let chunker = Chunker::new(config);
//!
//! let code = r#"
//! fn process_data(input: &str) -> Result<String> {
//!     let cleaned = input.trim();
//!     Ok(cleaned.to_uppercase())
//! }
//! "#;
//!
//! let chunks = chunker.chunk_str(code, Some("example.rs")).unwrap();
//! for chunk in chunks {
//!     println!("Chunk at lines {}-{}: {}",
//!              chunk.start_line, chunk.end_line, chunk.metadata.symbol_name.unwrap_or_default());
//! }
//! ```

mod ast_analyzer;
mod chunker;
mod config;
mod error;
mod language;
mod strategy;
mod types;

pub use chunker::Chunker;
pub use config::{ChunkerConfig, ChunkingStrategy, OverlapStrategy};
pub use error::{ChunkerError, Result};
pub use types::{ChunkMetadata, ChunkType, CodeChunk};
