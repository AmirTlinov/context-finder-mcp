use crate::ContextPackItem;
use context_indexer::AnchorKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedAnchor {
    pub kind: AnchorKind,
    pub raw: String,
    pub normalized: String,
}

const MIN_QUOTED_LEN: usize = 3;
const MIN_IDENTIFIER_LEN: usize = 4;
const MIN_PATH_LEN: usize = 3;

const STOPWORDS: &[&str] = &[
    // English.
    "a",
    "an",
    "and",
    "are",
    "as",
    "at",
    "be",
    "by",
    "for",
    "from",
    "how",
    "in",
    "is",
    "it",
    "of",
    "on",
    "or",
    "that",
    "the",
    "this",
    "to",
    "what",
    "when",
    "where",
    "why",
    "with",
    // Common code/query noise.
    "struct",
    "definition",
    "define",
    "defined",
    "fn",
    "function",
    "method",
    "class",
    "type",
    "enum",
    "trait",
    "impl",
    "module",
    "file",
    "path",
    "usage",
    "usages",
    "reference",
    "references",
    "find",
    "show",
    // Common repo/path noise.
    "bin",
    "crates",
    "doc",
    "docs",
    "lib",
    "src",
    "test",
    "tests",
    // Common extensions.
    "c",
    "cpp",
    "go",
    "h",
    "hpp",
    "java",
    "js",
    "json",
    "md",
    "mdx",
    "py",
    "rs",
    "toml",
    "ts",
    "yaml",
    "yml",
    // Russian.
    "в",
    "для",
    "и",
    "или",
    "как",
    "на",
    "по",
    "почему",
    "что",
    "где",
    "зачем",
];

#[must_use]
fn is_stopword(token: &str) -> bool {
    let token = token.trim();
    if token.is_empty() {
        return true;
    }
    let token = token.to_ascii_lowercase();
    STOPWORDS.iter().any(|w| w == &token)
}

#[must_use]
fn strip_wrapping_punct(token: &str) -> &str {
    token.trim().trim_matches(|ch: char| {
        matches!(
            ch,
            ',' | '.' | ';' | ':' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
        )
    })
}

#[must_use]
fn strip_line_suffix(token: &str) -> &str {
    let token = token.trim();
    if let Some((head, tail)) = token.rsplit_once("#L") {
        if !head.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            return head;
        }
    }
    if let Some((head, tail)) = token.rsplit_once(':') {
        if !head.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            return head;
        }
    }
    token
}

#[must_use]
fn normalize_path_like(token: &str) -> String {
    let normalized = token.replace('\\', "/");
    strip_line_suffix(&normalized).to_string()
}

#[must_use]
fn looks_path_like(token: &str) -> bool {
    let token = strip_wrapping_punct(token);
    let token = strip_line_suffix(token);
    if token.len() < MIN_PATH_LEN {
        return false;
    }
    if token.contains('/') || token.contains('\\') {
        return token.chars().any(|c| c.is_ascii_alphanumeric());
    }
    const EXT: &[&str] = &[
        ".rs", ".toml", ".md", ".mdx", ".json", ".yaml", ".yml", ".ts", ".tsx", ".js", ".jsx",
        ".py", ".go", ".java", ".proto",
    ];
    EXT.iter().any(|ext| token.ends_with(ext))
}

#[must_use]
fn looks_identifier_like(token: &str) -> bool {
    let token = strip_wrapping_punct(token);
    if token.len() < MIN_IDENTIFIER_LEN {
        return false;
    }
    if is_stopword(token) {
        return false;
    }
    if token.contains("::") {
        return true;
    }
    if token.contains('_') || token.contains('-') {
        return token.chars().any(|c| c.is_ascii_alphabetic());
    }
    let mut has_upper_internal = false;
    for (idx, ch) in token.chars().enumerate() {
        if idx > 0 && ch.is_ascii_uppercase() {
            has_upper_internal = true;
            break;
        }
    }
    has_upper_internal || token.chars().any(|c| c.is_ascii_digit())
}

fn extract_quoted(query: &str) -> Vec<String> {
    fn extract_for_quote(query: &str, quote: char) -> Vec<String> {
        let mut out = Vec::new();
        let mut start: Option<usize> = None;
        for (idx, ch) in query.char_indices() {
            if ch != quote {
                continue;
            }
            match start {
                None => start = Some(idx + ch.len_utf8()),
                Some(s) => {
                    if idx > s {
                        out.push(query[s..idx].to_string());
                    }
                    start = None;
                }
            }
        }
        out
    }

    let mut out = Vec::new();
    out.extend(extract_for_quote(query, '"'));
    out.extend(extract_for_quote(query, '\''));
    out.extend(extract_for_quote(query, '`'));
    out
}

#[must_use]
pub fn detect_primary_anchor(query: &str) -> Option<DetectedAnchor> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }

    // 1) Quoted strings (strongest intent signal).
    let quoted = extract_quoted(query)
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| s.len() >= MIN_QUOTED_LEN)
        .filter(|s| s.chars().any(|c| c.is_ascii_alphanumeric()))
        .max_by_key(|s| s.len());
    if let Some(value) = quoted {
        let normalized = value.clone();
        return Some(DetectedAnchor {
            kind: AnchorKind::Quoted,
            raw: value,
            normalized,
        });
    }

    // 2) Path-like tokens.
    let path_like = query
        .split_whitespace()
        .map(strip_wrapping_punct)
        .filter(|t| looks_path_like(t))
        .map(normalize_path_like)
        .filter(|t| t.len() >= MIN_PATH_LEN)
        .max_by_key(|t| t.len());
    if let Some(value) = path_like {
        let raw = value.clone();
        return Some(DetectedAnchor {
            kind: AnchorKind::Path,
            raw,
            normalized: value,
        });
    }

    // 3) Identifier-like tokens.
    let ident = query
        .split_whitespace()
        .map(strip_wrapping_punct)
        .filter(|t| looks_identifier_like(t))
        .map(|t| strip_line_suffix(t).to_string())
        .max_by_key(|t| t.len());
    if let Some(value) = ident {
        let normalized = value.clone();
        return Some(DetectedAnchor {
            kind: AnchorKind::Identifier,
            raw: value,
            normalized,
        });
    }

    None
}

#[must_use]
fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

#[must_use]
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[must_use]
fn contains_case_insensitive_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let haystack_lc = haystack.to_ascii_lowercase();
    let needle_lc = needle.to_ascii_lowercase();
    let mut start = 0usize;
    while let Some(pos) = haystack_lc[start..].find(&needle_lc) {
        let idx = start + pos;
        let before_ok = idx == 0 || !is_ident_byte(haystack_lc.as_bytes()[idx - 1]);
        let after_idx = idx.saturating_add(needle_lc.len());
        let after_ok =
            after_idx >= haystack_lc.len() || !is_ident_byte(haystack_lc.as_bytes()[after_idx]);
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

#[must_use]
pub fn item_mentions_anchor(item: &ContextPackItem, anchor: &DetectedAnchor) -> bool {
    match anchor.kind {
        AnchorKind::Quoted => contains_case_insensitive(&item.content, &anchor.normalized),
        AnchorKind::Path => {
            contains_case_insensitive(&item.file.replace('\\', "/"), &anchor.normalized)
                || contains_case_insensitive(&item.content, &anchor.normalized)
        }
        AnchorKind::Identifier => {
            let needle = anchor.normalized.as_str();
            if needle.contains("::") || needle.contains('.') || needle.contains('-') {
                contains_case_insensitive(&item.content, needle)
            } else {
                contains_case_insensitive_word(&item.content, needle)
            }
        }
    }
}

#[must_use]
pub fn count_anchor_hits(items: &[ContextPackItem], anchor: &DetectedAnchor) -> usize {
    items
        .iter()
        .filter(|item| item_mentions_anchor(item, anchor))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn dummy_item(content: &str) -> ContextPackItem {
        ContextPackItem {
            id: "x".to_string(),
            role: "primary".to_string(),
            file: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 1,
            symbol: None,
            chunk_type: None,
            score: 1.0,
            imports: Vec::new(),
            content: content.to_string(),
            relationship: None,
            distance: None,
        }
    }

    #[test]
    fn detects_identifier_anchor_from_camelcase() {
        let anchor = detect_primary_anchor("LintWarning struct definition").expect("anchor");
        assert_eq!(anchor.kind, AnchorKind::Identifier);
        assert_eq!(anchor.normalized, "LintWarning");
    }

    #[test]
    fn detects_quoted_anchor_first() {
        let anchor = detect_primary_anchor("find \"X11Window\" struct definition").expect("anchor");
        assert_eq!(anchor.kind, AnchorKind::Quoted);
        assert_eq!(anchor.normalized, "X11Window");
    }

    #[test]
    fn detects_path_like_anchor_and_strips_line_suffix() {
        let anchor = detect_primary_anchor("open crates/search/src/lib.rs:12").expect("anchor");
        assert_eq!(anchor.kind, AnchorKind::Path);
        assert_eq!(anchor.normalized, "crates/search/src/lib.rs");
    }

    #[test]
    fn counts_hits_in_items() {
        let anchor = DetectedAnchor {
            kind: AnchorKind::Identifier,
            raw: "X11Window".to_string(),
            normalized: "X11Window".to_string(),
        };
        let items = vec![
            dummy_item("pub struct X11Window {}"),
            dummy_item("impl SomethingElse {}"),
        ];
        assert_eq!(count_anchor_hits(&items, &anchor), 1);
    }

    proptest! {
        #[test]
        fn proptest_detects_quoted_anchor(value in "[A-Za-z0-9_]{3,32}") {
            let q = format!("find \"{value}\" struct definition");
            let anchor = detect_primary_anchor(&q).expect("anchor");
            prop_assert_eq!(anchor.kind, AnchorKind::Quoted);
            prop_assert_eq!(anchor.normalized, value);
        }

        #[test]
        fn proptest_detects_path_anchor_and_strips_line_suffix(line in 1u32..100000u32) {
            let q = format!("open crates/search/src/lib.rs:{line}");
            let anchor = detect_primary_anchor(&q).expect("anchor");
            prop_assert_eq!(anchor.kind, AnchorKind::Path);
            prop_assert_eq!(anchor.normalized, "crates/search/src/lib.rs");
        }

        #[test]
        fn proptest_identifier_anchor_respects_word_boundaries(
            needle in "[A-Za-z]{4,24}",
            before in "[A-Za-z0-9_]",
            after in "[A-Za-z0-9_]",
        ) {
            let anchor = DetectedAnchor {
                kind: AnchorKind::Identifier,
                raw: needle.clone(),
                normalized: needle.clone(),
            };
            // Surrounded by identifier characters -> should NOT count as a whole-word match.
            let content = format!("{before}{needle}{after}");
            let item = dummy_item(&content);
            prop_assert!(!item_mentions_anchor(&item, &anchor));
        }

        #[test]
        fn proptest_identifier_anchor_matches_when_separated(
            needle in "[A-Za-z]{4,24}",
            left_ws in "[ \\t]{1,3}",
            right_ws in "[ \\t]{1,3}",
        ) {
            let anchor = DetectedAnchor {
                kind: AnchorKind::Identifier,
                raw: needle.clone(),
                normalized: needle.clone(),
            };
            let content = format!("{left_ws}{needle}{right_ws}");
            let item = dummy_item(&content);
            prop_assert!(item_mentions_anchor(&item, &anchor));
        }
    }
}
