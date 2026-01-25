use super::super::FileSliceCursorV1;
use super::{ReadPackContext, ResponseMode};

pub(super) fn snippet_inner_max_chars(inner_max_chars: usize) -> usize {
    // Snippet-mode should stay small and leave room for envelope + cursor strings.
    let min_chars = 200usize;
    let max_chars = 2_000usize;
    (inner_max_chars / 3).clamp(min_chars, max_chars)
}

pub(super) fn resolve_file_slice_max_chars(
    ctx: &ReadPackContext,
    response_mode: ResponseMode,
    cursor_payload: Option<&FileSliceCursorV1>,
    request_max_chars: Option<usize>,
) -> usize {
    if let Some(decoded) = cursor_payload {
        if request_max_chars.is_some() {
            ctx.inner_max_chars
        } else {
            decoded.max_chars
        }
    } else {
        match response_mode {
            ResponseMode::Full => ctx.inner_max_chars,
            ResponseMode::Facts | ResponseMode::Minimal => {
                snippet_inner_max_chars(ctx.inner_max_chars)
            }
        }
    }
}
