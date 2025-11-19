//! # Context Indexer
//!
//! Project indexing for semantic code search.
//!
//! ## Pipeline
//!
//! ```text
//! Directory
//!     │
//!     ├──> File Scanner (.gitignore aware)
//!     │      └─> Source files
//!     │
//!     ├──> Chunker (AST-aware)
//!     │      └─> Code chunks
//!     │
//!     └──> Vector Store (batch embed)
//!            └─> Searchable index
//! ```
//!
//! ## Example
//!
//! ```no_run
//! use context_indexer::ProjectIndexer;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let indexer = ProjectIndexer::new("/path/to/project").await?;
//!     let stats = indexer.index().await?;
//!
//!     println!("Indexed {} files, {} chunks", stats.files, stats.chunks);
//!     Ok(())
//! }
//! ```

mod error;
mod indexer;
mod scanner;
mod stats;

pub use error::{IndexerError, Result};
pub use indexer::ProjectIndexer;
pub use stats::IndexStats;
