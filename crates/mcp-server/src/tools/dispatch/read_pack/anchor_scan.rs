use super::ReadPackSnippetKind;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Clone, Copy)]
struct AnchorNeedle {
    needle: &'static str,
    score: i32,
}

const MEMORY_ANCHOR_SCAN_MAX_LINES: usize = 2_000;
const MEMORY_ANCHOR_SCAN_MAX_BYTES: usize = 200_000;

const DOC_ANCHOR_NEEDLES: &[AnchorNeedle] = &[
    AnchorNeedle {
        needle: "cargo test",
        score: 120,
    },
    AnchorNeedle {
        needle: "pytest",
        score: 120,
    },
    AnchorNeedle {
        needle: "go test",
        score: 120,
    },
    AnchorNeedle {
        needle: "npm test",
        score: 120,
    },
    AnchorNeedle {
        needle: "yarn test",
        score: 120,
    },
    AnchorNeedle {
        needle: "pnpm test",
        score: 120,
    },
    AnchorNeedle {
        needle: "clippy",
        score: 110,
    },
    AnchorNeedle {
        needle: "cargo run",
        score: 105,
    },
    AnchorNeedle {
        needle: "npm run dev",
        score: 105,
    },
    AnchorNeedle {
        needle: "npm start",
        score: 105,
    },
    AnchorNeedle {
        needle: "quick start",
        score: 95,
    },
    AnchorNeedle {
        needle: "getting started",
        score: 95,
    },
    AnchorNeedle {
        needle: "project invariants",
        score: 110,
    },
    AnchorNeedle {
        needle: "invariants",
        score: 95,
    },
    AnchorNeedle {
        needle: "инвариант",
        score: 95,
    },
    AnchorNeedle {
        needle: "philosophy",
        score: 85,
    },
    AnchorNeedle {
        needle: "философ",
        score: 85,
    },
    AnchorNeedle {
        needle: "architecture",
        score: 85,
    },
    AnchorNeedle {
        needle: "архитектур",
        score: 85,
    },
    AnchorNeedle {
        needle: "protocol",
        score: 80,
    },
    AnchorNeedle {
        needle: "протокол",
        score: 80,
    },
    AnchorNeedle {
        needle: "contract",
        score: 80,
    },
    AnchorNeedle {
        needle: "контракт",
        score: 80,
    },
    AnchorNeedle {
        needle: "install",
        score: 70,
    },
    AnchorNeedle {
        needle: "usage",
        score: 60,
    },
    AnchorNeedle {
        needle: "configuration",
        score: 70,
    },
    AnchorNeedle {
        needle: ".env.example",
        score: 70,
    },
    AnchorNeedle {
        needle: "docker",
        score: 45,
    },
];

const CONFIG_ANCHOR_NEEDLES: &[AnchorNeedle] = &[
    AnchorNeedle {
        needle: "test",
        score: 80,
    },
    AnchorNeedle {
        needle: "lint",
        score: 70,
    },
    AnchorNeedle {
        needle: "clippy",
        score: 70,
    },
    AnchorNeedle {
        needle: "fmt",
        score: 60,
    },
    AnchorNeedle {
        needle: "format",
        score: 60,
    },
    AnchorNeedle {
        needle: "build",
        score: 60,
    },
    AnchorNeedle {
        needle: "run",
        score: 55,
    },
    AnchorNeedle {
        needle: "scripts",
        score: 80,
    },
    AnchorNeedle {
        needle: "run:",
        score: 55,
    },
];

const ENTRYPOINT_ANCHOR_NEEDLES: &[AnchorNeedle] = &[
    AnchorNeedle {
        needle: "fn main",
        score: 120,
    },
    AnchorNeedle {
        needle: "func main(",
        score: 120,
    },
    AnchorNeedle {
        needle: "def main",
        score: 120,
    },
    AnchorNeedle {
        needle: "public static void main",
        score: 120,
    },
    AnchorNeedle {
        needle: "int main(",
        score: 120,
    },
    AnchorNeedle {
        needle: "app.listen",
        score: 90,
    },
    AnchorNeedle {
        needle: "createserver",
        score: 80,
    },
];

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

pub(super) fn memory_best_start_line(
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

pub(super) fn best_anchor_line_for_kind(
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
