use super::{ReadPackSection, ReadPackSnippet, ReadPackSnippetKind};
use std::collections::HashMap;

pub(super) fn overlap_dedupe_snippet_sections(sections: &mut Vec<ReadPackSection>) {
    #[derive(Clone, Copy, Debug)]
    struct KeptSpan {
        idx: usize,
        start_line: usize,
        end_line: usize,
        priority: (u8, u8, usize),
    }

    let mut out: Vec<ReadPackSection> = Vec::with_capacity(sections.len());
    let mut kept_by_file: HashMap<String, Vec<KeptSpan>> = HashMap::new();

    for section in sections.drain(..) {
        let ReadPackSection::Snippet { result: snippet } = section else {
            out.push(section);
            continue;
        };

        // The memory "focus file" snippet is a UX anchor; never collapse it away.
        if snippet_is_focus_file(&snippet) {
            out.push(ReadPackSection::Snippet { result: snippet });
            continue;
        }

        let mut incoming = Some(snippet);
        let file = incoming.as_ref().expect("incoming set above").file.clone();
        let incoming_priority = snippet_priority(incoming.as_ref().expect("incoming set above"));
        let mut keep_incoming = true;

        let spans = kept_by_file.entry(file.clone()).or_default();
        for kept in spans.iter_mut() {
            let Some(existing_snippet) = (match out.get_mut(kept.idx) {
                Some(ReadPackSection::Snippet { result }) => Some(result),
                _ => None,
            }) else {
                continue;
            };

            if snippet_is_focus_file(existing_snippet) {
                continue;
            }

            let incoming_ref = incoming.as_ref().expect("incoming present");
            let Some(overlap) = snippet_overlap_len(existing_snippet, incoming_ref) else {
                continue;
            };

            // Exact duplicate span: keep the stronger one.
            if existing_snippet.start_line == incoming_ref.start_line
                && existing_snippet.end_line == incoming_ref.end_line
            {
                if incoming_priority > kept.priority {
                    if let Some(snippet) = incoming.take() {
                        *existing_snippet = snippet;
                    }
                    kept.start_line = existing_snippet.start_line;
                    kept.end_line = existing_snippet.end_line;
                    kept.priority = incoming_priority;
                }
                keep_incoming = false;
                break;
            }

            // Full containment: always drop the contained window (no information loss).
            let incoming_contains_existing = incoming_ref.start_line <= kept.start_line
                && incoming_ref.end_line >= kept.end_line;
            let existing_contains_incoming = kept.start_line <= incoming_ref.start_line
                && kept.end_line >= incoming_ref.end_line;

            if existing_contains_incoming {
                keep_incoming = false;
                break;
            }
            if incoming_contains_existing {
                if incoming_priority >= kept.priority {
                    if let Some(snippet) = incoming.take() {
                        *existing_snippet = snippet;
                    }
                    kept.start_line = existing_snippet.start_line;
                    kept.end_line = existing_snippet.end_line;
                    kept.priority = incoming_priority;
                }
                keep_incoming = false;
                break;
            }

            // Partial overlap: only collapse when it's mostly redundant; otherwise keep both so we
            // don't lose unique context (true merging is a future step).
            if !snippet_overlap_is_redundant(overlap, existing_snippet, incoming_ref) {
                continue;
            }

            if incoming_priority > kept.priority {
                if let Some(snippet) = incoming.take() {
                    *existing_snippet = snippet;
                }
                kept.start_line = existing_snippet.start_line;
                kept.end_line = existing_snippet.end_line;
                kept.priority = incoming_priority;
            }
            keep_incoming = false;
            break;
        }

        if keep_incoming {
            let Some(snippet) = incoming.take() else {
                continue;
            };
            let idx = out.len();
            spans.push(KeptSpan {
                idx,
                start_line: snippet.start_line,
                end_line: snippet.end_line,
                priority: incoming_priority,
            });
            out.push(ReadPackSection::Snippet { result: snippet });
        }
    }

    *sections = out;
}

pub(super) fn strip_snippet_reasons_for_output(
    sections: &mut [ReadPackSection],
    keep_focus_file: bool,
) {
    for section in sections {
        match section {
            ReadPackSection::Snippet { result } => {
                if keep_focus_file && snippet_is_focus_file(result) {
                    continue;
                }
                result.reason = None;
            }
            ReadPackSection::Recall { result } => {
                for snippet in &mut result.snippets {
                    snippet.reason = None;
                }
            }
            _ => {}
        }
    }
}

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

fn snippet_priority(snippet: &ReadPackSnippet) -> (u8, u8, usize) {
    let tier = snippet_reason_tier(snippet.reason.as_deref());
    let kind = snippet_kind_tier(snippet.kind);
    let span = snippet
        .end_line
        .saturating_sub(snippet.start_line)
        .saturating_add(1);
    (tier, kind, span)
}

fn snippet_overlap_len(a: &ReadPackSnippet, b: &ReadPackSnippet) -> Option<usize> {
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

fn snippet_is_focus_file(snippet: &ReadPackSnippet) -> bool {
    snippet.reason.as_deref() == Some(super::REASON_ANCHOR_FOCUS_FILE)
}

fn snippet_overlap_is_redundant(
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
