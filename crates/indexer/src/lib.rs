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
mod health;
mod index_lock;
mod index_state;
mod indexer;
mod limits;
mod scanner;
mod stats;
mod watcher;
mod watermark_io;

pub use error::{IndexerError, Result};
pub use health::append_failure_reason;
pub use health::{health_file_path, read_health_snapshot, write_health_snapshot, HealthSnapshot};
pub use index_lock::{index_write_lock_wait_ms_last, index_write_lock_wait_ms_max};
pub use index_state::{
    assess_staleness, root_fingerprint, IndexSnapshot, IndexState, ReindexAttempt, ReindexResult,
    StaleAssessment, StaleReason, ToolMeta, Watermark, INDEX_STATE_SCHEMA_VERSION,
};
pub use indexer::{ModelIndexSpec, MultiModelProjectIndexer, ProjectIndexer};
pub use limits::{index_concurrency_snapshot, IndexConcurrencySnapshot};
pub use scanner::{FileScanner, ScanOptions};
pub use stats::IndexStats;
pub use watcher::{
    IndexUpdate, IndexerHealth, MultiModelStreamingIndexer, StreamingIndexer,
    StreamingIndexerConfig,
};
pub use watermark_io::{
    compute_project_watermark, index_watermark_path_for_store, read_index_watermark,
    write_index_watermark, PersistedIndexWatermark,
};
