use super::super::{ReadPackSnippet, ReadPackSnippetKind, REASON_ANCHOR_FOCUS_FILE};

fn snippet_reason_tier(reason: Option<&str>) -> u8 {
    let Some(reason) = reason else { return 0 };
    let lower = reason.trim().to_ascii_lowercase();
    if lower.starts_with("needle:") {
        return 3;
    }
    if lower.starts_with("halo:") {
        return 2;
    }
    if lower.starts_with("anchor:") {
        return 1;
    }
    0
}

fn snippet_kind_tier(kind: Option<ReadPackSnippetKind>) -> u8 {
    match kind {
        Some(ReadPackSnippetKind::Code) => 3,
        Some(ReadPackSnippetKind::Config) => 2,
        Some(ReadPackSnippetKind::Doc) => 1,
        None => 0,
    }
}

pub(super) fn snippet_priority(snippet: &ReadPackSnippet) -> (u8, u8, usize) {
    let tier = snippet_reason_tier(snippet.reason.as_deref());
    let kind = snippet_kind_tier(snippet.kind);
    let span = snippet
        .end_line
        .saturating_sub(snippet.start_line)
        .saturating_add(1);
    (tier, kind, span)
}

pub(super) fn snippet_overlap_len(a: &ReadPackSnippet, b: &ReadPackSnippet) -> Option<usize> {
    if a.file != b.file {
        return None;
    }
    let start = a.start_line.max(b.start_line);
    let end = a.end_line.min(b.end_line);
    if start > end {
        return None;
    }
    Some(end.saturating_sub(start).saturating_add(1))
}

pub(super) fn snippet_is_focus_file(snippet: &ReadPackSnippet) -> bool {
    snippet.reason.as_deref() == Some(REASON_ANCHOR_FOCUS_FILE)
}

pub(super) fn snippet_overlap_is_redundant(
    overlap_lines: usize,
    a: &ReadPackSnippet,
    b: &ReadPackSnippet,
) -> bool {
    if overlap_lines == 0 {
        return false;
    }
    let a_len = a.end_line.saturating_sub(a.start_line).saturating_add(1);
    let b_len = b.end_line.saturating_sub(b.start_line).saturating_add(1);
    let smaller = a_len.min(b_len).max(1);
    // Redundancy heuristic: if most of the smaller snippet is already covered, prefer a single
    // window (saves budget and reduces "needle spam" in facts mode).
    overlap_lines.saturating_mul(100) >= smaller.saturating_mul(70)
}
