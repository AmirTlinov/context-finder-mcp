use super::budget::{estimate_match_cost, truncate_to_chars};
use super::types::{TextSearchOutcome, TextSearchSettings};

use crate::tools::dispatch::CallToolResult;
use crate::tools::dispatch::ContextFinderService;
use crate::tools::paths::normalize_relative_path;
use crate::tools::schemas::text_search::{TextSearchCursorModeV1, TextSearchMatch};
use crate::tools::secrets::is_potential_secret_path;
use context_indexer::FileScanner;
use context_indexer::ScanOptions;
use context_protocol::BudgetTruncation;
use std::path::{Path, PathBuf};

use super::super::error::invalid_cursor;

const MAX_FILE_BYTES: u64 = 2_000_000;

pub(super) fn search_in_filesystem(
    root: &Path,
    settings: &TextSearchSettings<'_>,
    allow_secrets: bool,
    content_max_chars: usize,
    start_file_index: usize,
    start_line_offset: usize,
) -> std::result::Result<TextSearchOutcome, CallToolResult> {
    let mut outcome = TextSearchOutcome::new();

    let scanner = FileScanner::new(root);
    let scan_options = if allow_secrets {
        ScanOptions {
            allow_hidden: true,
            allow_secrets: true,
        }
    } else {
        ScanOptions::default()
    };
    let mut candidates: Vec<(String, PathBuf)> = scanner
        .scan_with_options(scan_options)
        .into_iter()
        .filter_map(|file| normalize_relative_path(root, &file).map(|rel| (rel, file)))
        .filter(|(rel, _)| allow_secrets || !is_potential_secret_path(rel))
        .filter(|(rel, _)| ContextFinderService::matches_file_pattern(rel, settings.file_pattern))
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    if start_file_index > candidates.len() {
        return Err(invalid_cursor("Invalid cursor: out of range"));
    }

    'outer_fs: for (file_index, (rel_path, abs_path)) in
        candidates.iter().enumerate().skip(start_file_index)
    {
        if outcome.matches.len() >= settings.max_results {
            outcome.truncated = true;
            outcome.truncation = Some(BudgetTruncation::MaxMatches);
            outcome.next_state = Some(TextSearchCursorModeV1::Filesystem {
                file_index,
                line_offset: 0,
            });
            break 'outer_fs;
        }

        outcome.scanned_files += 1;

        let Ok(meta) = std::fs::metadata(abs_path) else {
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            outcome.skipped_large_files += 1;
            continue;
        }

        let Ok(content) = std::fs::read_to_string(abs_path) else {
            continue;
        };

        let first_file = file_index == start_file_index;
        let line_start = if first_file { start_line_offset } else { 0 };

        for (offset, line_text) in content.lines().enumerate().skip(line_start) {
            if outcome.matches.len() >= settings.max_results {
                outcome.truncated = true;
                outcome.truncation = Some(BudgetTruncation::MaxMatches);
                outcome.next_state = Some(TextSearchCursorModeV1::Filesystem {
                    file_index,
                    line_offset: offset,
                });
                break 'outer_fs;
            }

            let Some(col_byte) = ContextFinderService::match_in_line(
                line_text,
                settings.pattern,
                settings.case_sensitive,
                settings.whole_word,
            ) else {
                continue;
            };
            let column = line_text[..col_byte].chars().count() + 1;
            let mut item = TextSearchMatch {
                file: rel_path.clone(),
                line: offset + 1,
                column,
                text: line_text.to_string(),
            };

            let new_file = !outcome.matched_files.contains(&item.file);
            let cost = estimate_match_cost(&item.file, &item.text, new_file);
            if outcome.matches.is_empty() && cost > content_max_chars {
                let file_chars = if new_file {
                    item.file.chars().count()
                } else {
                    0
                };
                let fixed_overhead = 36usize + 24usize;
                let allowed_text = content_max_chars
                    .saturating_sub(file_chars.saturating_add(fixed_overhead))
                    .max(1);
                item.text = truncate_to_chars(&item.text, allowed_text);
                let truncated_cost = estimate_match_cost(&item.file, &item.text, new_file);
                if outcome.push_match(item) {
                    outcome.used_chars = outcome.used_chars.saturating_add(truncated_cost);
                }
                outcome.truncated = true;
                outcome.truncation = Some(BudgetTruncation::MaxChars);
                outcome.next_state = Some(TextSearchCursorModeV1::Filesystem {
                    file_index,
                    line_offset: offset.saturating_add(1),
                });
                break 'outer_fs;
            }

            if outcome.used_chars.saturating_add(cost) > content_max_chars {
                outcome.truncated = true;
                outcome.truncation = Some(BudgetTruncation::MaxChars);
                outcome.next_state = Some(TextSearchCursorModeV1::Filesystem {
                    file_index,
                    line_offset: offset,
                });
                break 'outer_fs;
            }

            if outcome.push_match(item) {
                outcome.used_chars = outcome.used_chars.saturating_add(cost);
            }
        }
    }

    Ok(outcome)
}
