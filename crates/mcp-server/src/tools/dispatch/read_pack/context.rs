use super::{ReadPackRequest, ToolResult};
use std::path::PathBuf;

pub(super) struct ReadPackContext {
    pub(super) root: PathBuf,
    pub(super) root_display: String,
    pub(super) max_chars: usize,
    pub(super) inner_max_chars: usize,
}

pub(super) fn build_context(
    request: &ReadPackRequest,
    root: PathBuf,
    root_display: String,
) -> ToolResult<ReadPackContext> {
    let max_chars = request
        .max_chars
        .unwrap_or(super::DEFAULT_MAX_CHARS)
        .clamp(super::MIN_MAX_CHARS, super::MAX_MAX_CHARS);
    // Inner tool budgets must leave headroom for output overhead (cursor strings, metadata).
    //
    // `.context` output is lightweight, so we reserve less and spend more budget on payload.
    let reserved_for_envelope = (max_chars / 10)
        .clamp(64, 800)
        .min(max_chars.saturating_sub(64));
    let inner_max_chars = max_chars
        .saturating_sub(reserved_for_envelope)
        .max(64)
        .min(max_chars);

    Ok(ReadPackContext {
        root,
        root_display,
        max_chars,
        inner_max_chars,
    })
}
