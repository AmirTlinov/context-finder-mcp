//! # Context Search
//!
//! Hybrid search combining semantic and fuzzy matching for code.
//!
//! ## Strategy
//!
//! ```text
//! Query
//!   ├─> Semantic (70%) - embeddings + cosine similarity
//!   ├─> Fuzzy (30%) - nucleo matcher for names/paths
//!   └─> RRF Fusion - Reciprocal Rank Fusion
//!         └─> AST-aware boost - prioritize functions > variables
//! ```

mod error;
mod fusion;
mod fuzzy;
mod hybrid;
mod query_expansion;

pub use error::{Result, SearchError};
pub use fusion::{AstBooster, RRFFusion};
pub use fuzzy::FuzzySearch;
pub use hybrid::HybridSearch;
pub use query_expansion::QueryExpander;

// Re-export for convenience
pub use context_vector_store::SearchResult;
