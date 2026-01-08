use anyhow::{Context as AnyhowContext, Result};
use context_indexer::{FileScanner, ToolMeta};
use context_protocol::enforce_max_chars;
use std::path::Path;

use super::cursor::{cursor_fingerprint, encode_cursor, CURSOR_VERSION};
use super::paths::normalize_relative_path;
use super::schemas::list_files::{ListFilesCursorV1, ListFilesResult, ListFilesTruncation};
use super::secrets::is_potential_secret_path;
use super::ContextFinderService;

fn list_files_content_budget(max_chars: usize) -> usize {
    const MIN_CONTENT_CHARS: usize = 120;
    const MAX_RESERVE_CHARS: usize = 4_096;

    // `.context` envelopes are intentionally tiny, but still need headroom for:
    // [CONTENT], A: line, provenance note, and an optional cursor block.
    let base_reserve = 120usize;
    let proportional = max_chars / 20;
    let mut reserve = base_reserve.max(proportional).min(MAX_RESERVE_CHARS);
    reserve = reserve.min(max_chars.saturating_sub(MIN_CONTENT_CHARS));
    max_chars.saturating_sub(reserve).max(1)
}

pub(super) fn decode_list_files_cursor(cursor: &str) -> Result<ListFilesCursorV1> {
    super::cursor::decode_cursor(cursor).with_context(|| "decode list_files cursor")
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn compute_list_files_result(
    root: &Path,
    root_display: &str,
    file_pattern: Option<&str>,
    limit: usize,
    max_chars: usize,
    allow_secrets: bool,
    cursor_last_file: Option<&str>,
) -> Result<ListFilesResult> {
    let file_pattern = file_pattern.map(str::trim).filter(|s| !s.is_empty());
    let cursor_last_file = cursor_last_file.map(str::trim).filter(|s| !s.is_empty());

    let content_max_chars = list_files_content_budget(max_chars);
    let mut used_chars = 0usize;
    let mut truncated = false;
    let mut truncation: Option<ListFilesTruncation> = None;
    let mut files: Vec<String> = Vec::new();
    let mut next_cursor: Option<String> = None;
    let mut matched: Vec<String> = Vec::new();

    // Agent-native UX: treat the filesystem as the source of truth.
    //
    // A corpus/index can be partial (scoped indexing, in-progress indexing, or project-specific
    // filters). Tight-loop read tools must behave like `rg/find` replacements and therefore must
    // not silently ignore files just because they're not in the corpus.
    let source = "filesystem".to_string();

    let scanner = FileScanner::new(root);
    let scanned_paths = scanner.scan();
    let scanned_files = scanned_paths.len();

    let mut candidates: Vec<String> = scanned_paths
        .into_iter()
        .filter_map(|p| normalize_relative_path(root, &p))
        .collect();
    candidates.sort();

    for file in candidates {
        if !ContextFinderService::matches_file_pattern(&file, file_pattern) {
            continue;
        }
        if !allow_secrets && is_potential_secret_path(&file) {
            continue;
        }
        matched.push(file);
    }

    let start_index = cursor_last_file.map_or(0, |last| {
        match matched.binary_search_by(|candidate| candidate.as_str().cmp(last)) {
            Ok(idx) => idx + 1,
            Err(idx) => idx,
        }
    });

    if start_index > matched.len() {
        anyhow::bail!("Cursor is out of range for matched files");
    }

    for file in matched.iter().skip(start_index) {
        if files.len() >= limit {
            truncated = true;
            truncation = Some(ListFilesTruncation::MaxItems);
            break;
        }

        let file_chars = file.chars().count();
        // `.context` renders file paths line-by-line. Count the newline explicitly so `used_chars`
        // matches what the agent sees.
        let extra_chars = file_chars.saturating_add(1);
        if used_chars.saturating_add(extra_chars) > content_max_chars {
            truncated = true;
            truncation = Some(ListFilesTruncation::MaxChars);
            break;
        }

        files.push(file.clone());
        used_chars += extra_chars;
    }

    if truncated && start_index.saturating_add(files.len()) < matched.len() {
        // Cursor-first contract: even when an extremely small budget can't fit a single path,
        // return a cursor that allows the agent to retry with a larger budget without skipping.
        let last_file = files
            .last()
            .cloned()
            .or_else(|| cursor_last_file.map(str::to_string))
            .unwrap_or_default();
        next_cursor = Some(encode_cursor(&ListFilesCursorV1 {
            v: CURSOR_VERSION,
            tool: "list_files".to_string(),
            root: Some(root_display.to_string()),
            root_hash: Some(cursor_fingerprint(root_display)),
            file_pattern: file_pattern.map(str::to_string),
            limit,
            max_chars,
            allow_secrets,
            last_file,
        })?);
    }

    Ok(ListFilesResult {
        source: Some(source),
        file_pattern: file_pattern.map(str::to_string),
        scanned_files: Some(scanned_files),
        returned: Some(files.len()),
        used_chars: Some(used_chars),
        limit: Some(limit),
        max_chars: Some(max_chars),
        truncated,
        truncation,
        next_cursor,
        next_actions: None,
        meta: Some(ToolMeta::default()),
        files,
    })
}

pub(super) fn finalize_list_files_budget(
    result: &mut ListFilesResult,
    max_chars: usize,
) -> Result<()> {
    fn reanchor_cursor(result: &mut ListFilesResult) -> Result<()> {
        let Some(cursor) = result.next_cursor.as_deref() else {
            return Ok(());
        };
        let mut decoded = decode_list_files_cursor(cursor)?;
        if let Some(last_file) = result.files.last() {
            decoded.last_file = last_file.clone();
        }
        result.next_cursor = Some(encode_cursor(&decoded)?);
        Ok(())
    }

    let include_used_chars = result.used_chars.is_some();

    if include_used_chars {
        let used = enforce_max_chars(
            result,
            max_chars,
            |inner, used| inner.used_chars = Some(used),
            |inner| {
                inner.truncated = true;
                inner.truncation = Some(ListFilesTruncation::MaxChars);
            },
            |inner| {
                if !inner.files.is_empty() {
                    inner.files.pop();
                    if let Some(returned) = inner.returned.as_mut() {
                        *returned = inner.files.len();
                    }
                    return true;
                }
                false
            },
        )?;
        result.used_chars = Some(used);
        reanchor_cursor(result)?;
        return Ok(());
    }

    loop {
        let raw = serde_json::to_string(result)?;
        let used = raw.chars().count();
        if used <= max_chars {
            reanchor_cursor(result)?;
            return Ok(());
        }

        result.truncated = true;
        result.truncation = Some(ListFilesTruncation::MaxChars);
        if !result.files.is_empty() {
            result.files.pop();
            if let Some(returned) = result.returned.as_mut() {
                *returned = result.files.len();
            }
            continue;
        }
        reanchor_cursor(result)?;
        anyhow::bail!("budget exceeded (used_chars={used}, max_chars={max_chars})");
    }
}
