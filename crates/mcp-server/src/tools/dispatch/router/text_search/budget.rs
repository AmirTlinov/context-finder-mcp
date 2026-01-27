use crate::tools::schemas::response_mode::ResponseMode;

pub(super) fn text_search_content_budget(max_chars: usize, response_mode: ResponseMode) -> usize {
    const MIN_CONTENT_CHARS: usize = 120;
    const MAX_RESERVE_CHARS: usize = 4_096;

    let (base_reserve, divisor) = (
        match response_mode {
            // `.context` envelopes are intentionally tiny; reserve just enough headroom for:
            // [CONTENT], A:/R: lines, and an optional cursor block.
            ResponseMode::Minimal => 80,
            ResponseMode::Facts => 100,
            ResponseMode::Full => 320,
        },
        20,
    );

    // Reserve for the JSON envelope + per-match metadata (file/line/column).
    let proportional = max_chars / divisor;
    let mut reserve = base_reserve.max(proportional).min(MAX_RESERVE_CHARS);
    reserve = reserve.min(max_chars.saturating_sub(MIN_CONTENT_CHARS));
    max_chars.saturating_sub(reserve).max(1)
}

pub(super) fn truncate_to_chars(input: &str, max_chars: usize) -> String {
    let max_chars = max_chars.max(1);
    let mut cut_byte = input.len();
    for (seen, (idx, _)) in input.char_indices().enumerate() {
        if seen == max_chars {
            cut_byte = idx;
            break;
        }
    }
    input[..cut_byte].to_string()
}

pub(super) fn estimate_match_cost(file: &str, text: &str, new_file: bool) -> usize {
    // Conservative approximation of how much a match contributes to the serialized output.
    // We purposely over-estimate to preserve the "budget-first" contract.
    // `.context` format groups matches by file:
    // - first match in a file pays for the `R:` header (incl. file path) + one match line
    // - subsequent matches pay only for a match line (no repeated file path)
    const HEADER_OVERHEAD_CHARS: usize = 36;
    const MATCH_LINE_OVERHEAD_CHARS: usize = 24;

    let file_chars = if new_file { file.chars().count() } else { 0 };
    file_chars
        + text.chars().count()
        + MATCH_LINE_OVERHEAD_CHARS
        + if new_file { HEADER_OVERHEAD_CHARS } else { 0 }
}
