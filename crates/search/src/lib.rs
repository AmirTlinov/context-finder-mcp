mod anchor;
mod context_pack;
mod context_search;
mod error;
mod fusion;
mod fuzzy;
pub mod hybrid;
mod multi;
pub mod profile;
mod rerank;
mod task_pack;
pub use context_vector_store::SearchResult;
mod query_classifier;
mod query_expansion;

pub use anchor::{count_anchor_hits, detect_primary_anchor, item_mentions_anchor, DetectedAnchor};
pub use context_pack::{
    ContextPackBudget, ContextPackItem, ContextPackOutput, CONTEXT_PACK_VERSION,
};
pub use context_search::{ContextSearch, EnrichedResult, RelatedContext};
pub use error::{Result, SearchError};
pub use fusion::{AstBooster, RRFFusion};
pub use fuzzy::FuzzySearch;
pub use hybrid::HybridSearch;
pub use multi::{MultiModelContextSearch, MultiModelHybridSearch};
pub use profile::{Bm25Config, MatchKind, RerankConfig, SearchProfile, Thresholds};
pub use query_classifier::{QueryClassifier, QueryType, QueryWeights};
pub use query_expansion::QueryExpander;
pub use task_pack::{NextAction, NextActionKind, TaskPackItem, TaskPackOutput, TASK_PACK_VERSION};
