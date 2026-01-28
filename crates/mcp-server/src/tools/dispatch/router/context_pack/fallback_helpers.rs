use crate::tools::schemas::grep_context::GrepContextHunk;
use std::path::Path;

use super::inputs::ContextPackInputs;
use context_search::ContextPackItem;

pub(super) fn choose_fallback_token(tokens: &[String]) -> Option<String> {
    fn is_low_value(token_lc: &str) -> bool {
        matches!(
            token_lc,
            "struct"
                | "definition"
                | "define"
                | "defined"
                | "fn"
                | "function"
                | "method"
                | "class"
                | "type"
                | "enum"
                | "trait"
                | "impl"
                | "module"
                | "file"
                | "path"
                | "usage"
                | "usages"
                | "reference"
                | "references"
                | "what"
                | "where"
                | "find"
                | "show"
        )
    }

    let mut best: Option<String> = None;
    for token in tokens {
        let token = token.trim();
        if token.len() < 4 {
            continue;
        }
        let token_lc = token.to_lowercase();
        if is_low_value(&token_lc) {
            continue;
        }
        let looks_like_identifier = token
            .chars()
            .any(|ch| ch.is_ascii_uppercase() || ch == '_' || ch == '-');
        if !looks_like_identifier && token.len() < 8 {
            continue;
        }
        if best.as_ref().is_none_or(|b| token.len() > b.len()) {
            best = Some(token.to_string());
        }
    }

    best.or_else(|| {
        tokens
            .iter()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .max_by_key(|t| t.len())
            .map(|t| t.to_string())
    })
}

pub(super) fn items_mention_token(items: &[ContextPackItem], token: &str) -> bool {
    let token = token.trim();
    if token.is_empty() {
        return true;
    }
    let token_lc = token.to_lowercase();
    items.iter().take(6).any(|item| {
        item.symbol
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(token))
            || item.file.contains(token)
            || item.file.to_lowercase().contains(&token_lc)
            || item.content.contains(token)
            || item.content.to_lowercase().contains(&token_lc)
    })
}

pub(super) async fn collect_scoped_fallback_hunks(
    root: &Path,
    root_display: &str,
    pattern: &str,
    inputs: &ContextPackInputs,
    max_hunks: usize,
    max_chars: usize,
) -> anyhow::Result<Vec<GrepContextHunk>> {
    // Respect user-provided context_pack filters (best-effort) to avoid scanning unrelated areas.
    //
    // GrepContext currently supports only a single file_pattern, so for multiple include_paths we
    // scan each prefix (bounded) until we collect enough hunks.
    let include_prefixes = inputs
        .include_paths
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>();
    let exclude_prefixes = inputs
        .exclude_paths
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>();

    let mut patterns_to_scan: Vec<Option<&str>> = Vec::new();
    if let Some(fp) = inputs.file_pattern.as_deref() {
        patterns_to_scan.push(Some(fp));
    } else if !include_prefixes.is_empty() {
        // Bound repeated scans to avoid worst-case latency blowups.
        for p in include_prefixes.iter().take(5) {
            patterns_to_scan.push(Some(p));
        }
    } else {
        patterns_to_scan.push(None);
    }

    let mut all_hunks = Vec::new();
    let mut remaining_hunks = max_hunks;
    let mut remaining_chars = max_chars;
    for file_pattern in patterns_to_scan {
        if remaining_hunks == 0 || remaining_chars == 0 {
            break;
        }

        let hunks = super::super::semantic_fallback::grep_fallback_hunks_scoped(
            root,
            root_display,
            pattern,
            file_pattern,
            inputs.response_mode,
            remaining_hunks,
            remaining_chars,
        )
        .await?;

        for h in hunks {
            if all_hunks.len() >= max_hunks {
                break;
            }
            if !include_prefixes.is_empty()
                && !include_prefixes.iter().any(|p| h.file.starts_with(*p))
            {
                continue;
            }
            if exclude_prefixes.iter().any(|p| h.file.starts_with(*p)) {
                continue;
            }
            let content_chars = h.content.chars().count();
            if content_chars > remaining_chars {
                continue;
            }
            remaining_chars = remaining_chars.saturating_sub(content_chars);
            remaining_hunks = remaining_hunks.saturating_sub(1);
            all_hunks.push(h);
        }

        if all_hunks.len() >= max_hunks {
            break;
        }
    }

    Ok(all_hunks)
}
