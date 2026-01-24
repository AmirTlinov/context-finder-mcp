use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::util::truncate_to_chars;
use context_protocol::path_filters;
use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

const DOC_CONTEXT_CANDIDATES: &[&str] = &[
    "AGENTS.md",
    "README.md",
    "docs/README.md",
    "docs/QUICK_START.md",
    "ARCHITECTURE.md",
    "docs/ARCHITECTURE.md",
    "PHILOSOPHY.md",
    "docs/PHILOSOPHY.md",
    "CONTRIBUTING.md",
    "DEVELOPMENT.md",
    "docs/DEVELOPMENT.md",
    "docs/QUALITY_CHARTER.md",
];

const MAX_DOC_SCAN_LINES: usize = 1_200;
const MAX_DOC_SCAN_BYTES: usize = 200_000;
const MAX_SNIPPET_LINES: usize = 12;
const MAX_SNIPPET_CHARS: usize = 900;
const MAX_SNIPPETS: usize = 2;
const MAX_FALLBACK_DOCS: usize = 6;
const MAX_DIR_ENTRIES: usize = 40;
const MAX_TOKENS: usize = 8;
const MIN_TOKEN_LEN: usize = 2;
const MIN_SCORE: i32 = 40;

#[derive(Debug, Clone)]
pub(crate) struct DocSnippet {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
}

pub(crate) fn collect_doc_context(
    root: &Path,
    raw_tokens: &[String],
    include_paths: &[String],
    exclude_paths: &[String],
    file_pattern: Option<&str>,
) -> Vec<DocSnippet> {
    let tokens = normalize_tokens(raw_tokens);
    if tokens.is_empty() {
        return Vec::new();
    }

    let filters_active = path_filters::is_active(include_paths, exclude_paths, file_pattern);
    let candidates = collect_doc_candidates(
        root,
        filters_active,
        include_paths,
        exclude_paths,
        file_pattern,
    );
    if candidates.is_empty() {
        return Vec::new();
    }

    let mut matches: Vec<(i32, String, usize)> = Vec::new();
    for rel in candidates {
        if let Some((score, line)) = scan_doc_for_tokens(root, &rel, &tokens) {
            if score >= MIN_SCORE {
                matches.push((score, rel, line));
            }
        }
    }

    matches.sort_by(|(a_score, a_path, a_line), (b_score, b_path, b_line)| {
        b_score
            .cmp(a_score)
            .then_with(|| a_path.cmp(b_path))
            .then_with(|| a_line.cmp(b_line))
    });

    matches
        .into_iter()
        .take(MAX_SNIPPETS)
        .filter_map(|(_, rel, line)| read_doc_snippet(root, &rel, line))
        .collect()
}

pub(crate) fn push_doc_context(doc: &mut ContextDocBuilder, snippets: &[DocSnippet]) {
    for snippet in snippets {
        doc.push_note(&format!(
            "doc_context: {}:{}-{}",
            snippet.file, snippet.start_line, snippet.end_line
        ));
        doc.push_block_smart(&snippet.content);
        doc.push_blank();
    }
}

fn collect_doc_candidates(
    root: &Path,
    filters_active: bool,
    include_paths: &[String],
    exclude_paths: &[String],
    file_pattern: Option<&str>,
) -> Vec<String> {
    let mut candidates: Vec<String> = DOC_CONTEXT_CANDIDATES
        .iter()
        .filter_map(|rel| {
            let rel_norm = rel.replace('\\', "/");
            let full = root.join(&rel_norm);
            if full.is_file() {
                Some(rel_norm)
            } else {
                None
            }
        })
        .filter(|rel| {
            !filters_active
                || path_filters::path_allowed(rel, include_paths, exclude_paths, file_pattern)
        })
        .collect();

    if candidates.is_empty() && !filters_active {
        candidates = collect_fallback_doc_candidates(root);
    }

    candidates
}

fn normalize_tokens(tokens: &[String]) -> Vec<String> {
    let mut out: Vec<String> = tokens
        .iter()
        .map(|token| token.trim().to_lowercase())
        .filter(|token| token.len() >= MIN_TOKEN_LEN)
        .filter(|token| token.chars().any(|ch| ch.is_alphanumeric()))
        .collect();

    out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    out.dedup();
    out.truncate(MAX_TOKENS);
    out
}

fn scan_doc_for_tokens(root: &Path, rel: &str, tokens: &[String]) -> Option<(i32, usize)> {
    let full = root.join(rel);
    let file = File::open(&full).ok()?;
    let mut reader = BufReader::new(file);

    let mut best: Option<(i32, usize)> = None;
    let mut line = String::new();
    let mut bytes_read = 0usize;
    let mut line_no = 0usize;
    let min_matches = required_match_count(tokens);

    loop {
        line.clear();
        let read = reader.read_line(&mut line).ok()?;
        if read == 0 {
            break;
        }
        bytes_read = bytes_read.saturating_add(read);
        if bytes_read > MAX_DOC_SCAN_BYTES || line_no >= MAX_DOC_SCAN_LINES {
            break;
        }
        line_no = line_no.saturating_add(1);

        let (score, matches) = score_line(&line, tokens);
        if matches < min_matches || score < MIN_SCORE {
            continue;
        }
        let replace = match best {
            None => true,
            Some((best_score, best_line)) => {
                score > best_score || (score == best_score && line_no < best_line)
            }
        };
        if replace {
            best = Some((score, line_no));
        }
    }

    best
}

fn required_match_count(tokens: &[String]) -> usize {
    if tokens.is_empty() {
        return 0;
    }
    let strong = tokens.iter().filter(|t| is_strong_token(t)).count();
    if strong > 0 {
        1
    } else {
        2.min(tokens.len())
    }
}

fn score_line(line: &str, tokens: &[String]) -> (i32, usize) {
    let lowered = line.to_lowercase();
    let mut score = 0i32;
    let mut matches = 0usize;

    for token in tokens {
        if lowered.contains(token) {
            matches = matches.saturating_add(1);
            score = score.saturating_add(token_score(token));
        }
    }

    if matches == 0 {
        return (0, 0);
    }

    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        score = score.saturating_add(20);
    }

    (score, matches)
}

fn token_score(token: &str) -> i32 {
    let len = token.chars().count().min(12) as i32;
    let base = if is_strong_token(token) { 30 } else { 10 };
    base + len.saturating_mul(3)
}

fn read_doc_snippet(root: &Path, rel: &str, anchor_line: usize) -> Option<DocSnippet> {
    let full = root.join(rel);
    let file = File::open(&full).ok()?;
    let mut reader = BufReader::new(file);

    let start_line = anchor_line.saturating_sub(MAX_SNIPPET_LINES / 3).max(1);
    let end_line = start_line
        .saturating_add(MAX_SNIPPET_LINES)
        .saturating_sub(1);

    let mut line_no = 0usize;
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();

    loop {
        line.clear();
        let read = reader.read_line(&mut line).ok()?;
        if read == 0 {
            break;
        }
        line_no = line_no.saturating_add(1);
        if line_no < start_line {
            continue;
        }
        if line_no > end_line {
            break;
        }
        lines.push(line.trim_end_matches(&['\n', '\r'][..]).to_string());
    }

    if lines.is_empty() {
        return None;
    }

    let mut trimmed_start = start_line;
    while lines.first().is_some_and(|value| value.trim().is_empty()) {
        lines.remove(0);
        trimmed_start = trimmed_start.saturating_add(1);
    }
    while lines.last().is_some_and(|value| value.trim().is_empty()) {
        lines.pop();
    }

    if lines.is_empty() {
        return None;
    }

    let actual_start = trimmed_start;
    let actual_end = actual_start.saturating_add(lines.len()).saturating_sub(1);
    let mut content = lines.join("\n");
    if content.chars().count() > MAX_SNIPPET_CHARS {
        let max_chars = MAX_SNIPPET_CHARS.saturating_sub(3).max(1);
        content = truncate_to_chars(&content, max_chars);
        content.push_str("...");
    }

    Some(DocSnippet {
        file: rel.to_string(),
        start_line: actual_start,
        end_line: actual_end,
        content,
    })
}

fn is_strong_token(token: &str) -> bool {
    let len = token.chars().count();
    len >= 6
        || token.chars().any(|ch| !ch.is_alphanumeric())
        || token.chars().any(|ch| ch.is_uppercase())
        || token.chars().any(|ch| ch.is_numeric())
}

fn collect_fallback_doc_candidates(root: &Path) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut candidates: Vec<(i32, String)> = Vec::new();
    for rel_dir in ["", "docs", "doc", "documentation"] {
        let dir = if rel_dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(rel_dir)
        };
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.take(MAX_DIR_ENTRIES) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let ty = match entry.file_type() {
                Ok(ty) => ty,
                Err(_) => continue,
            };
            if !ty.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let rel = if rel_dir.is_empty() {
                name
            } else {
                format!("{rel_dir}/{name}")
            };
            let rel_norm = rel.replace('\\', "/");
            if !is_doc_context_candidate(&rel_norm) {
                continue;
            }
            if !seen.insert(rel_norm.clone()) {
                continue;
            }
            let score = fallback_doc_score(&rel_norm);
            candidates.push((score, rel_norm));
        }
    }

    candidates.sort_by(|(a_score, a_rel), (b_score, b_rel)| {
        b_score.cmp(a_score).then_with(|| a_rel.cmp(b_rel))
    });
    candidates.truncate(MAX_FALLBACK_DOCS);
    candidates.into_iter().map(|(_, rel)| rel).collect()
}

fn is_doc_context_candidate(rel: &str) -> bool {
    let ext = Path::new(rel)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "md" | "mdx" | "rst" | "adoc" | "txt" | "context"
    )
}

fn fallback_doc_score(rel: &str) -> i32 {
    let normalized = rel.replace('\\', "/").to_ascii_lowercase();
    let name = normalized.rsplit('/').next().unwrap_or(normalized.as_str());

    if name.starts_with("agents.") {
        return 220;
    }
    if name.starts_with("readme") {
        return 210;
    }
    if name.contains("quick") || name.contains("getting_started") {
        return 190;
    }
    if name.contains("architecture") {
        return 180;
    }
    if name.contains("philosophy") || name.contains("invariants") {
        return 170;
    }
    if name.contains("contributing") || name.contains("development") {
        return 165;
    }
    if name.contains("guide") || name.contains("usage") || name.contains("setup") {
        return 160;
    }
    120
}
