//! # Context Vector Store
//!
//! High-performance vector storage and similarity search for code embeddings.
//!
//! ## Features
//!
//! - **Fast ANN search** via HNSW (Hierarchical Navigable Small World)
//! - **Efficient embeddings** using FastEmbed
//! - **Persistent storage** with JSON serialization
//! - **Incremental updates** for dynamic codebases
//! - **Batch operations** for optimal performance
//!
//! ## Architecture
//!
//! ```text
//! CodeChunk[]
//!     │
//!     ├──> Embedding Model (FastEmbed)
//!     │      └─> Vector[384/768/1024]
//!     │
//!     ├──> HNSW Index
//!     │      └─> Fast ANN Search
//!     │
//!     └──> Persistent Storage
//!            └─> JSON/Binary Format
//! ```
//!
//! ## Example
//!
//! ```no_run
//! use context_vector_store::VectorStore;
//! use context_code_chunker::CodeChunk;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let mut store = VectorStore::new("vectors.json").await?;
//!
//!     // Add chunks
//!     let chunks = vec![/* CodeChunk instances */];
//!     store.add_chunks(chunks).await?;
//!
//!     // Search
//!     let results = store.search("async function error handling", 10).await?;
//!
//!     for result in results {
//!         println!("{}: {:.3}", result.chunk.file_path, result.score);
//!     }
//!
//!     Ok(())
//! }
//! ```

mod embeddings;
mod error;
mod hnsw_index;
mod store;
mod types;

pub use embeddings::EmbeddingModel;
pub use error::{Result, VectorStoreError};
pub use store::VectorStore;
pub use types::{SearchResult, StoredChunk};

// Re-export code chunker types for convenience
pub use context_code_chunker::{ChunkMetadata, ChunkType, CodeChunk};
