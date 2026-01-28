use crate::tools::dispatch::router::error::invalid_request;
use crate::tools::dispatch::{QueryType, RelatedMode};

use super::super::ToolResult;

pub(super) fn parse_strategy(
    raw: Option<&str>,
    docs_intent: bool,
    query_type: QueryType,
) -> context_graph::AssemblyStrategy {
    match raw {
        Some("direct") => context_graph::AssemblyStrategy::Direct,
        Some("deep") => context_graph::AssemblyStrategy::Deep,
        Some(_) => context_graph::AssemblyStrategy::Extended,
        None => {
            if !docs_intent && matches!(query_type, QueryType::Identifier | QueryType::Path) {
                context_graph::AssemblyStrategy::Direct
            } else {
                context_graph::AssemblyStrategy::Extended
            }
        }
    }
}

pub(super) fn parse_related_mode(
    raw: Option<&str>,
    docs_intent: bool,
    query_type: QueryType,
) -> ToolResult<RelatedMode> {
    let default = if !docs_intent && matches!(query_type, QueryType::Identifier | QueryType::Path) {
        "focus"
    } else {
        "explore"
    };
    match raw.unwrap_or(default) {
        "explore" => Ok(RelatedMode::Explore),
        "focus" => Ok(RelatedMode::Focus),
        _ => Err(invalid_request(
            "Error: related_mode must be 'explore' or 'focus'",
        )),
    }
}
