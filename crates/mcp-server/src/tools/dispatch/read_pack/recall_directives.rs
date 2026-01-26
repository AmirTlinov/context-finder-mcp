use super::candidates::is_disallowed_memory_file;
use super::cursors::limits::{MAX_RECALL_FILTER_PATH_BYTES, MAX_RECALL_SNIPPETS_PER_QUESTION};
use super::cursors::trim_utf8_bytes;
use super::recall::parse_path_token;
use std::path::Path;

pub(super) fn parse_recall_regex_directive(question: &str) -> Option<String> {
    let q = question.trim();
    let lowered = q.to_ascii_lowercase();
    for prefix in ["re:", "regex:"] {
        if lowered.starts_with(prefix) {
            let rest = q[prefix.len()..].trim();
            if rest.is_empty() {
                return None;
            }
            return Some(rest.to_string());
        }
    }
    None
}

pub(super) fn parse_recall_literal_directive(question: &str) -> Option<String> {
    let q = question.trim();
    let lowered = q.to_ascii_lowercase();
    for prefix in ["lit:", "literal:"] {
        if lowered.starts_with(prefix) {
            let rest = q[prefix.len()..].trim();
            if rest.is_empty() {
                return None;
            }
            return Some(rest.to_string());
        }
    }
    None
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum RecallQuestionMode {
    #[default]
    Auto,
    Fast,
    Deep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RecallQuestionPolicy {
    pub(super) allow_semantic: bool,
}

pub(super) fn recall_question_policy(
    mode: RecallQuestionMode,
    semantic_index_fresh: bool,
) -> RecallQuestionPolicy {
    let allow_semantic = match mode {
        RecallQuestionMode::Fast => false,
        RecallQuestionMode::Deep => true,
        RecallQuestionMode::Auto => semantic_index_fresh,
    };

    RecallQuestionPolicy { allow_semantic }
}

#[derive(Debug, Default)]
pub(super) struct RecallQuestionDirectives {
    pub(super) mode: RecallQuestionMode,
    pub(super) snippet_limit: Option<usize>,
    pub(super) grep_context: Option<usize>,
    pub(super) include_paths: Vec<String>,
    pub(super) exclude_paths: Vec<String>,
    pub(super) file_pattern: Option<String>,
    pub(super) file_ref: Option<(String, Option<usize>)>,
}

fn normalize_recall_directive_prefix(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let (token, _line) = parse_path_token(raw)?;
    let token = trim_utf8_bytes(&token, MAX_RECALL_FILTER_PATH_BYTES);
    if token.is_empty() || token == "." || token.starts_with('/') || token.contains("..") {
        return None;
    }
    Some(token)
}

fn normalize_recall_directive_pattern(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let token = raw.replace('\\', "/");
    let token = token.strip_prefix("./").unwrap_or(&token);
    if token.is_empty() || token.starts_with('/') || token.contains("..") {
        return None;
    }
    Some(trim_utf8_bytes(token, MAX_RECALL_FILTER_PATH_BYTES))
}

fn parse_duration_ms_token(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let lowered = raw.to_ascii_lowercase();
    if let Some(value) = lowered.strip_suffix("ms") {
        return value.trim().parse::<u64>().ok();
    }
    if let Some(value) = lowered.strip_suffix('s') {
        let secs = value.trim().parse::<u64>().ok()?;
        return secs.checked_mul(1_000);
    }

    lowered.parse::<u64>().ok()
}

pub(super) fn parse_recall_question_directives(
    question: &str,
    root: &Path,
) -> (String, RecallQuestionDirectives) {
    const MAX_DIRECTIVE_PREFIXES: usize = 4;

    let mut directives = RecallQuestionDirectives::default();
    let mut remaining: Vec<&str> = Vec::new();

    for token in question.split_whitespace() {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }

        let lowered = token.to_ascii_lowercase();

        match lowered.as_str() {
            "fast" | "quick" | "grep" => {
                directives.mode = RecallQuestionMode::Fast;
                continue;
            }
            "deep" | "semantic" | "sem" | "index" => {
                directives.mode = RecallQuestionMode::Deep;
                continue;
            }
            _ => {}
        }

        if let Some(rest) = lowered
            .strip_prefix("index:")
            .or_else(|| lowered.strip_prefix("deep:"))
        {
            if parse_duration_ms_token(rest).is_some() {
                directives.mode = RecallQuestionMode::Deep;
                continue;
            }
        }

        if let Some(rest) = lowered
            .strip_prefix("k:")
            .or_else(|| lowered.strip_prefix("snips:"))
            .or_else(|| lowered.strip_prefix("top:"))
        {
            if let Ok(k) = rest.trim().parse::<usize>() {
                directives.snippet_limit = Some(k.clamp(1, MAX_RECALL_SNIPPETS_PER_QUESTION));
                continue;
            }
        }

        if let Some(rest) = lowered
            .strip_prefix("ctx:")
            .or_else(|| lowered.strip_prefix("context:"))
        {
            if let Ok(lines) = rest.trim().parse::<usize>() {
                directives.grep_context = Some(lines.clamp(0, 40));
                continue;
            }
        }

        let include_prefixes = ["in:", "scope:"];
        if include_prefixes.iter().any(|p| lowered.starts_with(p)) {
            if directives.include_paths.len() < MAX_DIRECTIVE_PREFIXES {
                let prefix_len = include_prefixes
                    .iter()
                    .find(|p| lowered.starts_with(*p))
                    .map(|p| p.len())
                    .unwrap_or(0);
                if let Some(prefix) =
                    normalize_recall_directive_prefix(token.get(prefix_len..).unwrap_or(""))
                {
                    directives.include_paths.push(prefix);
                }
            }
            continue;
        }

        let exclude_prefixes = ["not:", "out:", "exclude:"];
        if exclude_prefixes.iter().any(|p| lowered.starts_with(p)) {
            if directives.exclude_paths.len() < MAX_DIRECTIVE_PREFIXES {
                let prefix_len = exclude_prefixes
                    .iter()
                    .find(|p| lowered.starts_with(*p))
                    .map(|p| p.len())
                    .unwrap_or(0);
                if let Some(prefix) =
                    normalize_recall_directive_prefix(token.get(prefix_len..).unwrap_or(""))
                {
                    directives.exclude_paths.push(prefix);
                }
            }
            continue;
        }

        let pattern_prefixes = ["fp:", "glob:"];
        if pattern_prefixes.iter().any(|p| lowered.starts_with(p)) {
            let prefix_len = pattern_prefixes
                .iter()
                .find(|p| lowered.starts_with(*p))
                .map(|p| p.len())
                .unwrap_or(0);
            directives.file_pattern =
                normalize_recall_directive_pattern(token.get(prefix_len..).unwrap_or(""));
            continue;
        }

        let file_prefixes = ["file:", "open:"];
        if file_prefixes.iter().any(|p| lowered.starts_with(p)) {
            let prefix_len = file_prefixes
                .iter()
                .find(|p| lowered.starts_with(*p))
                .map(|p| p.len())
                .unwrap_or(0);
            let Some((candidate, line)) = parse_path_token(token.get(prefix_len..).unwrap_or(""))
            else {
                continue;
            };
            if is_disallowed_memory_file(&candidate) {
                continue;
            }

            if root.join(&candidate).is_file() {
                directives.file_ref = Some((candidate, line));
            }
            continue;
        }

        remaining.push(token);
    }

    let cleaned = remaining.join(" ").trim().to_string();
    (cleaned, directives)
}

pub(super) fn build_semantic_query(question: &str, topics: Option<&Vec<String>>) -> String {
    let Some(topics) = topics else {
        return question.to_string();
    };
    if topics.is_empty() {
        return question.to_string();
    }

    let joined = topics.join(", ");
    format!("{question}\n\nTopics: {joined}")
}
