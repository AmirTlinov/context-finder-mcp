//! # Context Vector Store
//!
//! High-performance vector storage and similarity search for code embeddings.
//!
//! ## Features
//!
//! - **Fast ANN search** via HNSW (Hierarchical Navigable Small World)
//! - **Efficient embeddings** using ONNX Runtime (CUDA)
//! - **Persistent storage** with JSON serialization
//! - **Incremental updates** for dynamic codebases
//! - **Batch operations** for optimal performance
//!
//! ## Architecture
//!
//! ```text
//! CodeChunk[]
//!     │
//!     ├──> Embedding Model (ONNX Runtime CUDA)
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
//!     let mut store = VectorStore::new("vectors.json")?;
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

mod corpus;
mod embedding_cache;
mod embeddings;
mod error;
pub mod gpu_env;
mod graph_node_store;
mod hnsw_index;
mod paths;
mod store;
mod templates;
mod types;

pub use corpus::{corpus_path_for_project_root, ChunkCorpus, CHUNK_CORPUS_SCHEMA_VERSION};
pub use embeddings::current_model_id;
pub use embeddings::model_dir;
pub use embeddings::EmbeddingModel;
pub use embeddings::{EmbedRequest, ModelRegistry};
pub use error::{Result, VectorStoreError};
pub use graph_node_store::{
    GraphNodeDoc, GraphNodeHit, GraphNodeStore, GraphNodeStoreMeta, GRAPH_NODE_STORE_SCHEMA_VERSION,
};
pub use paths::{
    context_dir_for_project_root, find_context_dir_from_path, is_context_dir_name,
    CONTEXT_CACHE_DIR_NAME, CONTEXT_DIR_NAME, LEGACY_CONTEXT_CACHE_DIR_NAME,
    LEGACY_CONTEXT_DIR_NAME,
};
pub use store::VectorIndex;
pub use store::VectorStore;
pub use templates::{
    classify_document_kind, classify_path_kind, DocumentKind, EmbeddingTemplates,
    GraphNodeTemplates, QueryKind, QueryTemplates, EMBEDDING_TEMPLATES_SCHEMA_VERSION,
};
pub use types::{SearchResult, StoredChunk};

// Re-export code chunker types for convenience
pub use context_code_chunker::{ChunkMetadata, ChunkType, CodeChunk};
