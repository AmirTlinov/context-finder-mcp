mod error;
mod fuzzy;
mod fusion;
mod hybrid;
mod query_expansion;
mod context_search;

pub use error::{Result, SearchError};
pub use fusion::{AstBooster, RRFFusion};
pub use fuzzy::FuzzySearch;
pub use hybrid::HybridSearch;
pub use query_expansion::QueryExpander;
pub use context_search::{ContextSearch, EnrichedResult, RelatedContext};
