use super::super::ReadPackSnippetKind;
use super::needles::{
    AnchorNeedle, CONFIG_ANCHOR_NEEDLES, DOC_ANCHOR_NEEDLES, ENTRYPOINT_ANCHOR_NEEDLES,
    MEMORY_ANCHOR_SCAN_MAX_BYTES, MEMORY_ANCHOR_SCAN_MAX_LINES,
};
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Clone, Copy, Debug)]
enum AnchorScanMode {
    Plain,
    Markdown,
}

fn scan_best_anchor_line(
    root: &Path,
    rel: &str,
    needles: &[AnchorNeedle],
    mode: AnchorScanMode,
) -> Option<usize> {
    let path = root.join(rel);
    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut best_score = 0i32;
    let mut best_line: Option<usize> = None;
    let mut scanned_bytes = 0usize;
    let mut in_fenced_block = false;

    for (idx, line) in reader.lines().enumerate() {
        let line_no = idx + 1;
        if line_no > MEMORY_ANCHOR_SCAN_MAX_LINES {
            break;
        }
        let Ok(line) = line else {
            break;
        };
        scanned_bytes = scanned_bytes.saturating_add(line.len() + 1);
        if scanned_bytes > MEMORY_ANCHOR_SCAN_MAX_BYTES {
            break;
        }

        if matches!(mode, AnchorScanMode::Markdown) {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                in_fenced_block = !in_fenced_block;
                continue;
            }
            if in_fenced_block {
                continue;
            }
        }

        let lowered = line.to_ascii_lowercase();
        let mut score = 0i32;
        for needle in needles {
            if lowered.contains(needle.needle) {
                score = score.saturating_add(needle.score);
            }
        }

        // Slightly prefer headings when all else is equal: they tend to be stable navigation anchors.
        if lowered.starts_with('#') {
            let bonus = if matches!(mode, AnchorScanMode::Markdown) {
                30
            } else {
                5
            };
            score = score.saturating_add(bonus);
        }

        let replace = match best_line {
            None => score > 0,
            Some(existing) => score > best_score || (score == best_score && line_no < existing),
        };
        if replace {
            best_score = score;
            best_line = Some(line_no);
        }
    }

    best_line
}

pub(in crate::tools::dispatch::read_pack) fn memory_best_start_line(
    root: &Path,
    rel: &str,
    max_lines: usize,
    kind: ReadPackSnippetKind,
) -> usize {
    if rel.eq_ignore_ascii_case("AGENTS.md") || rel.eq_ignore_ascii_case("AGENTS.context") {
        return 1;
    }

    let (needles, mode) = match kind {
        ReadPackSnippetKind::Doc => (DOC_ANCHOR_NEEDLES, AnchorScanMode::Markdown),
        ReadPackSnippetKind::Config => (CONFIG_ANCHOR_NEEDLES, AnchorScanMode::Plain),
        ReadPackSnippetKind::Code => (ENTRYPOINT_ANCHOR_NEEDLES, AnchorScanMode::Plain),
    };

    let Some(anchor) = scan_best_anchor_line(root, rel, needles, mode) else {
        return 1;
    };

    anchor.saturating_sub(max_lines / 3).max(1)
}

pub(in crate::tools::dispatch::read_pack) fn best_anchor_line_for_kind(
    root: &Path,
    rel: &str,
    kind: ReadPackSnippetKind,
) -> Option<usize> {
    let (needles, mode) = match kind {
        ReadPackSnippetKind::Doc => (DOC_ANCHOR_NEEDLES, AnchorScanMode::Markdown),
        ReadPackSnippetKind::Config => (CONFIG_ANCHOR_NEEDLES, AnchorScanMode::Plain),
        ReadPackSnippetKind::Code => (ENTRYPOINT_ANCHOR_NEEDLES, AnchorScanMode::Plain),
    };
    scan_best_anchor_line(root, rel, needles, mode)
}
