use anyhow::{Context as AnyhowContext, Result};
use context_indexer::ToolMeta;
use context_protocol::enforce_max_chars;
use std::path::{Path, PathBuf};

use super::cursor::{cursor_fingerprint, encode_cursor, CURSOR_VERSION};
use super::paths::normalize_relative_path;
use super::schemas::ls::{LsCursorV1, LsResult, LsTruncation};
use super::secrets::is_potential_secret_path;

fn ls_content_budget(max_chars: usize) -> usize {
    const MIN_CONTENT_CHARS: usize = 120;
    const MAX_RESERVE_CHARS: usize = 4_096;

    // Keep headroom for: envelope, minimal diagnostics, and a continuation cursor block.
    let base_reserve = 120usize;
    let proportional = max_chars / 20;
    let mut reserve = base_reserve.max(proportional).min(MAX_RESERVE_CHARS);
    reserve = reserve.min(max_chars.saturating_sub(MIN_CONTENT_CHARS));
    max_chars.saturating_sub(reserve).max(1)
}

pub(super) fn decode_ls_cursor(cursor: &str) -> Result<LsCursorV1> {
    super::cursor::decode_cursor(cursor).with_context(|| "decode ls cursor")
}

fn resolve_candidate_dir(root: &Path, dir: &str) -> PathBuf {
    // dir is relative to root. We canonicalize and root-check later.
    root.join(Path::new(dir))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn compute_ls_result(
    root: &Path,
    root_display: &str,
    dir: &str,
    limit: usize,
    max_chars: usize,
    all: bool,
    allow_secrets: bool,
    cursor_last_entry: Option<&str>,
) -> Result<LsResult> {
    let dir = dir.trim();
    let dir = if dir.is_empty() { "." } else { dir };
    let cursor_last_entry = cursor_last_entry.map(str::trim).filter(|s| !s.is_empty());

    let candidate = resolve_candidate_dir(root, dir);
    let canonical_dir = candidate
        .canonicalize()
        .with_context(|| format!("Invalid dir '{dir}': failed to resolve"))?;
    if !canonical_dir.starts_with(root) {
        anyhow::bail!("Dir '{dir}' is outside project root");
    }
    let meta =
        std::fs::metadata(&canonical_dir).with_context(|| format!("Failed to stat dir '{dir}'"))?;
    if !meta.is_dir() {
        anyhow::bail!("Path '{dir}' is not a directory");
    }

    let display_dir =
        normalize_relative_path(root, &canonical_dir).unwrap_or_else(|| dir.to_string());
    let display_dir = if display_dir.is_empty() {
        ".".to_string()
    } else {
        display_dir
    };

    let content_max_chars = ls_content_budget(max_chars);
    let mut used_chars = 0usize;
    let mut truncated = false;
    let mut truncation: Option<LsTruncation> = None;
    let mut entries: Vec<String> = Vec::new();
    let mut next_cursor: Option<String> = None;

    // Agent-native UX: treat the filesystem as the source of truth.
    let source = "filesystem".to_string();

    let mut candidates: Vec<String> = std::fs::read_dir(&canonical_dir)
        .with_context(|| format!("Failed to list dir '{display_dir}'"))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|s| s.to_string())
                .or_else(|| {
                    let lossy = entry.file_name().to_string_lossy().into_owned();
                    if lossy.is_empty() {
                        None
                    } else {
                        Some(lossy)
                    }
                })
        })
        .filter(|name| all || !name.starts_with('.'))
        .filter(|name| {
            if allow_secrets {
                return true;
            }
            let rel = if display_dir == "." {
                name.to_string()
            } else {
                format!("{display_dir}/{name}")
            };
            !is_potential_secret_path(&rel)
        })
        .collect();
    candidates.sort();

    let start_index = cursor_last_entry.map_or(0, |last| {
        match candidates.binary_search_by(|candidate| candidate.as_str().cmp(last)) {
            Ok(idx) => idx + 1,
            Err(idx) => idx,
        }
    });

    if start_index > candidates.len() {
        anyhow::bail!("Cursor is out of range for directory entries");
    }

    for name in candidates.iter().skip(start_index) {
        if entries.len() >= limit {
            truncated = true;
            truncation = Some(LsTruncation::MaxItems);
            break;
        }

        let name_chars = name.chars().count();
        let extra_chars = name_chars.saturating_add(1);
        if used_chars.saturating_add(extra_chars) > content_max_chars {
            truncated = true;
            truncation = Some(LsTruncation::MaxChars);
            break;
        }

        entries.push(name.clone());
        used_chars += extra_chars;
    }

    if truncated && start_index.saturating_add(entries.len()) < candidates.len() {
        let last_entry = entries
            .last()
            .cloned()
            .or_else(|| cursor_last_entry.map(str::to_string))
            .unwrap_or_default();
        next_cursor = Some(encode_cursor(&LsCursorV1 {
            v: CURSOR_VERSION,
            tool: "ls".to_string(),
            root: Some(root_display.to_string()),
            root_hash: Some(cursor_fingerprint(root_display)),
            dir: display_dir.clone(),
            all,
            allow_secrets,
            limit,
            max_chars,
            last_entry,
        })?);
    }

    Ok(LsResult {
        source: Some(source),
        dir: Some(display_dir),
        returned: Some(entries.len()),
        used_chars: Some(used_chars),
        limit: Some(limit),
        max_chars: Some(max_chars),
        truncated,
        truncation,
        next_cursor,
        next_actions: None,
        meta: Some(ToolMeta::default()),
        entries,
    })
}

pub(super) fn finalize_ls_budget(result: &mut LsResult, max_chars: usize) -> Result<()> {
    fn reanchor_cursor(result: &mut LsResult) -> Result<()> {
        let Some(cursor) = result.next_cursor.as_deref() else {
            return Ok(());
        };
        let mut decoded = decode_ls_cursor(cursor)?;
        if let Some(last) = result.entries.last() {
            decoded.last_entry = last.clone();
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
                inner.truncation = Some(LsTruncation::MaxChars);
            },
            |inner| {
                if !inner.entries.is_empty() {
                    inner.entries.pop();
                    if let Some(returned) = inner.returned.as_mut() {
                        *returned = inner.entries.len();
                    }
                    return true;
                }
                false
            },
        )?;
        result.used_chars = Some(used);
        reanchor_cursor(result)?;
        Ok(())
    } else {
        loop {
            let raw = serde_json::to_string(result)?;
            let used = raw.chars().count();
            if used <= max_chars {
                reanchor_cursor(result)?;
                return Ok(());
            }

            result.truncated = true;
            result.truncation = Some(LsTruncation::MaxChars);
            if !result.entries.is_empty() {
                result.entries.pop();
                if let Some(returned) = result.returned.as_mut() {
                    *returned = result.entries.len();
                }
                continue;
            }

            // Under extremely small budgets, keep cursor semantics intact but provide an empty list.
            result.entries.clear();
            result.returned = Some(0);
            reanchor_cursor(result)?;
            return Ok(());
        }
    }
}
