use super::super::super::{
    tokenize_focus_query, QueryClassifier, QueryType, RelatedMode, ResponseMode,
};
use super::super::error::invalid_request;
use super::ToolResult;
use crate::tools::schemas::context_pack::ContextPackRequest;

#[path = "inputs_parse.rs"]
mod parse;

use self::parse::{parse_related_mode, parse_strategy};

#[derive(Clone, Copy, Debug)]
pub(super) struct ContextPackFlags(pub(super) u8);

impl ContextPackFlags {
    const TRACE: u8 = 1 << 0;
    const INCLUDE_DOCS: u8 = 1 << 1;
    const PREFER_CODE: u8 = 1 << 2;

    pub(super) const fn trace(self) -> bool {
        self.0 & Self::TRACE != 0
    }

    pub(super) const fn include_docs(self) -> bool {
        self.0 & Self::INCLUDE_DOCS != 0
    }

    pub(super) const fn prefer_code(self) -> bool {
        self.0 & Self::PREFER_CODE != 0
    }
}

#[derive(Clone, Debug)]
pub(super) struct ContextPackInputs {
    pub(super) path: Option<String>,
    pub(super) format_version: u32,
    pub(super) limit: usize,
    pub(super) max_chars: usize,
    pub(super) max_related_per_primary: usize,
    pub(super) include_paths: Vec<String>,
    pub(super) exclude_paths: Vec<String>,
    pub(super) file_pattern: Option<String>,
    pub(super) flags: ContextPackFlags,
    pub(super) response_mode: ResponseMode,
    pub(super) query_type: QueryType,
    pub(super) strategy: context_graph::AssemblyStrategy,
    pub(super) related_mode: RelatedMode,
    pub(super) candidate_limit: usize,
    pub(super) query_tokens: Vec<String>,
}

pub(super) fn parse_inputs(request: &ContextPackRequest) -> ToolResult<ContextPackInputs> {
    if request.query.trim().is_empty() {
        return Err(invalid_request("Error: Query cannot be empty"));
    }

    let format_version = request.format_version.unwrap_or(1);
    if format_version != 1 && format_version != 2 {
        return Err(invalid_request(
            "Error: format_version must be 1 (default) or 2 (trust-first envelope)",
        ));
    }

    let limit = request.limit.unwrap_or(10).clamp(1, 50);
    let max_chars = request.max_chars.unwrap_or(6_000).max(1_000);
    let max_related_per_primary = request.max_related_per_primary.unwrap_or(3).clamp(0, 12);
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let trace = request.trace.unwrap_or(false) && response_mode == ResponseMode::Full;

    let query_type = QueryClassifier::classify(&request.query);
    let docs_intent = QueryClassifier::is_docs_intent(&request.query);
    let strategy = parse_strategy(request.strategy.as_deref(), docs_intent, query_type);

    let include_docs = request.include_docs.unwrap_or(true);
    let prefer_code = request.prefer_code.unwrap_or(!docs_intent);
    let related_mode =
        parse_related_mode(request.related_mode.as_deref(), docs_intent, query_type)?;

    let include_paths = request.include_paths.clone().unwrap_or_default();
    let exclude_paths = request.exclude_paths.clone().unwrap_or_default();
    let file_pattern = request
        .file_pattern
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(ToString::to_string);

    let candidate_limit = if include_docs && !prefer_code {
        limit.saturating_add(100).min(300)
    } else {
        limit.saturating_add(50).min(200)
    };
    let query_tokens = tokenize_focus_query(&request.query);
    let flags = {
        let mut bits = 0u8;
        if trace {
            bits |= ContextPackFlags::TRACE;
        }
        if include_docs {
            bits |= ContextPackFlags::INCLUDE_DOCS;
        }
        if prefer_code {
            bits |= ContextPackFlags::PREFER_CODE;
        }
        ContextPackFlags(bits)
    };

    Ok(ContextPackInputs {
        path: request.path.clone(),
        format_version,
        limit,
        max_chars,
        max_related_per_primary,
        include_paths,
        exclude_paths,
        file_pattern,
        flags,
        response_mode,
        query_type,
        strategy,
        related_mode,
        candidate_limit,
        query_tokens,
    })
}
