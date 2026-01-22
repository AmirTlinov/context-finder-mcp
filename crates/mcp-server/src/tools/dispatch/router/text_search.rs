use super::super::{
    decode_cursor, encode_cursor, normalize_relative_path, CallToolResult, Content,
    ContextFinderService, FileScanner, McpError, ResponseMode, TextSearchCursorModeV1,
    TextSearchCursorV1, TextSearchMatch, TextSearchRequest, TextSearchResult, ToolMeta,
    CURSOR_VERSION,
};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::cursor::cursor_fingerprint;
use crate::tools::schemas::ToolNextAction;
use crate::tools::secrets::is_potential_secret_path;
use context_indexer::{root_fingerprint, ScanOptions};
use context_protocol::BudgetTruncation;
use context_vector_store::ChunkCorpus;
use serde_json::json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 2_000_000;

use super::cursor_alias::{compact_cursor_alias, expand_cursor_alias};
use super::error::{
    attach_meta, attach_structured_content, internal_error, internal_error_with_meta,
    invalid_cursor, invalid_cursor_with_meta, invalid_cursor_with_meta_details,
    invalid_request_with_meta, meta_for_request,
};

fn trimmed_non_empty_str(input: Option<&str>) -> Option<&str> {
    input.map(str::trim).filter(|value| !value.is_empty())
}

fn text_search_content_budget(max_chars: usize, response_mode: ResponseMode) -> usize {
    const MIN_CONTENT_CHARS: usize = 120;
    const MAX_RESERVE_CHARS: usize = 4_096;

    let (base_reserve, divisor) = (
        match response_mode {
            // `.context` envelopes are intentionally tiny; reserve just enough headroom for:
            // [CONTENT], A:/R: lines, and an optional cursor block.
            ResponseMode::Minimal => 80,
            ResponseMode::Facts => 100,
            ResponseMode::Full => 320,
        },
        20,
    );

    // Reserve for the JSON envelope + per-match metadata (file/line/column).
    let proportional = max_chars / divisor;
    let mut reserve = base_reserve.max(proportional).min(MAX_RESERVE_CHARS);
    reserve = reserve.min(max_chars.saturating_sub(MIN_CONTENT_CHARS));
    max_chars.saturating_sub(reserve).max(1)
}

fn truncate_to_chars(input: &str, max_chars: usize) -> String {
    let max_chars = max_chars.max(1);
    let mut cut_byte = input.len();
    for (seen, (idx, _)) in input.char_indices().enumerate() {
        if seen == max_chars {
            cut_byte = idx;
            break;
        }
    }
    input[..cut_byte].to_string()
}

fn estimate_match_cost(file: &str, text: &str, new_file: bool) -> usize {
    // Conservative approximation of how much a match contributes to the serialized output.
    // We purposely over-estimate to preserve the "budget-first" contract.
    // `.context` format groups matches by file:
    // - first match in a file pays for the `R:` header (incl. file path) + one match line
    // - subsequent matches pay only for a match line (no repeated file path)
    const HEADER_OVERHEAD_CHARS: usize = 36;
    const MATCH_LINE_OVERHEAD_CHARS: usize = 24;

    let file_chars = if new_file { file.chars().count() } else { 0 };
    file_chars
        + text.chars().count()
        + MATCH_LINE_OVERHEAD_CHARS
        + if new_file { HEADER_OVERHEAD_CHARS } else { 0 }
}

struct TextSearchSettings<'a> {
    pattern: &'a str,
    file_pattern: Option<&'a str>,
    max_results: usize,
    max_chars: usize,
    case_sensitive: bool,
    whole_word: bool,
}

struct TextSearchOutcome {
    matches: Vec<TextSearchMatch>,
    matched_files: HashSet<String>,
    seen: HashSet<TextSearchKey>,
    scanned_files: usize,
    skipped_large_files: usize,
    truncated: bool,
    truncation: Option<BudgetTruncation>,
    used_chars: usize,
    next_state: Option<TextSearchCursorModeV1>,
}

#[derive(Hash, PartialEq, Eq)]
struct TextSearchKey {
    file: String,
    line: usize,
    column: usize,
    text: String,
}

impl TextSearchOutcome {
    fn new() -> Self {
        Self {
            matches: Vec::new(),
            matched_files: HashSet::new(),
            seen: HashSet::new(),
            scanned_files: 0,
            skipped_large_files: 0,
            truncated: false,
            truncation: None,
            used_chars: 0,
            next_state: None,
        }
    }

    fn push_match(&mut self, item: TextSearchMatch) -> bool {
        let key = TextSearchKey {
            file: item.file.clone(),
            line: item.line,
            column: item.column,
            text: item.text.clone(),
        };
        if !self.seen.insert(key) {
            return false;
        }
        self.matched_files.insert(item.file.clone());
        self.matches.push(item);
        true
    }
}

fn decode_cursor_payload(
    request: &TextSearchRequest,
    root_display: &str,
    requested_allow_secrets: Option<bool>,
    meta: &ToolMeta,
) -> std::result::Result<Option<TextSearchCursorV1>, CallToolResult> {
    let Some(cursor) = request
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };

    let decoded: TextSearchCursorV1 = match decode_cursor(cursor) {
        Ok(v) => v,
        Err(err) => {
            return Err(invalid_cursor_with_meta(
                format!("Invalid cursor: {err}"),
                meta.clone(),
            ))
        }
    };

    if decoded.v != CURSOR_VERSION || decoded.tool != "text_search" {
        return Err(invalid_cursor_with_meta(
            "Invalid cursor: wrong tool",
            meta.clone(),
        ));
    }
    if let Some(hash) = decoded.root_hash {
        if hash != cursor_fingerprint(root_display) {
            let expected_root_fingerprint = meta
                .root_fingerprint
                .unwrap_or_else(|| root_fingerprint(root_display));
            return Err(invalid_cursor_with_meta_details(
                "Invalid cursor: different root",
                meta.clone(),
                json!({
                    "expected_root_fingerprint": expected_root_fingerprint,
                    "cursor_root_fingerprint": Some(hash),
                }),
            ));
        }
    } else if decoded.root.as_deref() != Some(root_display) {
        let expected_root_fingerprint = meta
            .root_fingerprint
            .unwrap_or_else(|| root_fingerprint(root_display));
        let cursor_root_fingerprint = decoded.root.as_deref().map(root_fingerprint);
        return Err(invalid_cursor_with_meta_details(
            "Invalid cursor: different root",
            meta.clone(),
            json!({
                "expected_root_fingerprint": expected_root_fingerprint,
                "cursor_root_fingerprint": cursor_root_fingerprint,
            }),
        ));
    }

    if let Some(allow_secrets) = requested_allow_secrets {
        if decoded.allow_secrets != allow_secrets {
            return Err(invalid_cursor_with_meta(
                "Invalid cursor: different allow_secrets",
                meta.clone(),
            ));
        }
    }

    Ok(Some(decoded))
}

fn start_indices_for_corpus(
    cursor_mode: Option<&TextSearchCursorModeV1>,
) -> std::result::Result<(usize, usize, usize), CallToolResult> {
    match cursor_mode {
        None => Ok((0, 0, 0)),
        Some(TextSearchCursorModeV1::Corpus {
            file_index,
            chunk_index,
            line_offset,
        }) => Ok((*file_index, *chunk_index, *line_offset)),
        Some(TextSearchCursorModeV1::Filesystem { .. }) => {
            Err(invalid_cursor("Invalid cursor: wrong mode"))
        }
    }
}

fn start_indices_for_filesystem(
    cursor_mode: Option<&TextSearchCursorModeV1>,
) -> std::result::Result<(usize, usize), CallToolResult> {
    match cursor_mode {
        None => Ok((0, 0)),
        Some(TextSearchCursorModeV1::Filesystem {
            file_index,
            line_offset,
        }) => Ok((*file_index, *line_offset)),
        Some(TextSearchCursorModeV1::Corpus {
            file_index,
            chunk_index: _,
            line_offset,
        }) => Ok((*file_index, *line_offset)),
    }
}

fn encode_next_cursor(
    root_display: &str,
    settings: &TextSearchSettings<'_>,
    normalized_file_pattern: Option<&String>,
    allow_secrets: bool,
    mode: TextSearchCursorModeV1,
) -> std::result::Result<String, CallToolResult> {
    let token = TextSearchCursorV1 {
        v: CURSOR_VERSION,
        tool: "text_search".to_string(),
        root: Some(root_display.to_string()),
        root_hash: Some(cursor_fingerprint(root_display)),
        pattern: settings.pattern.to_string(),
        max_results: settings.max_results,
        max_chars: settings.max_chars,
        file_pattern: normalized_file_pattern.cloned(),
        case_sensitive: settings.case_sensitive,
        whole_word: settings.whole_word,
        allow_secrets,
        mode,
    };

    encode_cursor(&token).map_err(|err| internal_error(format!("Error: {err:#}")))
}

fn search_in_corpus(
    corpus: &ChunkCorpus,
    settings: &TextSearchSettings<'_>,
    content_max_chars: usize,
    start_file_index: usize,
    start_chunk_index: usize,
    start_line_offset: usize,
) -> std::result::Result<TextSearchOutcome, CallToolResult> {
    let mut outcome = TextSearchOutcome::new();

    let mut files: Vec<(&String, &Vec<context_code_chunker::CodeChunk>)> =
        corpus.files().iter().collect();
    files.sort_by(|a, b| a.0.cmp(b.0));
    files.retain(|(file, _)| {
        ContextFinderService::matches_file_pattern(file, settings.file_pattern)
    });

    if start_file_index > files.len() {
        return Err(invalid_cursor("Invalid cursor: out of range"));
    }

    'outer_corpus: for (file_index, (_file, chunks)) in
        files.iter().enumerate().skip(start_file_index)
    {
        if outcome.matches.len() >= settings.max_results {
            outcome.truncated = true;
            outcome.truncation = Some(BudgetTruncation::MaxMatches);
            outcome.next_state = Some(TextSearchCursorModeV1::Corpus {
                file_index,
                chunk_index: 0,
                line_offset: 0,
            });
            break 'outer_corpus;
        }

        outcome.scanned_files += 1;

        let mut chunk_refs: Vec<&context_code_chunker::CodeChunk> = chunks.iter().collect();
        chunk_refs.sort_by(|a, b| {
            a.start_line
                .cmp(&b.start_line)
                .then_with(|| a.end_line.cmp(&b.end_line))
        });

        let first_file = file_index == start_file_index;
        let start_chunk = if first_file { start_chunk_index } else { 0 };
        if start_chunk > chunk_refs.len() {
            return Err(invalid_cursor("Invalid cursor: out of range"));
        }

        for (chunk_index, chunk) in chunk_refs.iter().enumerate().skip(start_chunk) {
            if outcome.matches.len() >= settings.max_results {
                outcome.truncated = true;
                outcome.truncation = Some(BudgetTruncation::MaxMatches);
                outcome.next_state = Some(TextSearchCursorModeV1::Corpus {
                    file_index,
                    chunk_index,
                    line_offset: 0,
                });
                break 'outer_corpus;
            }

            let line_start = if first_file && chunk_index == start_chunk {
                start_line_offset
            } else {
                0
            };

            for (offset, line_text) in chunk.content.lines().enumerate().skip(line_start) {
                if outcome.matches.len() >= settings.max_results {
                    outcome.truncated = true;
                    outcome.truncation = Some(BudgetTruncation::MaxMatches);
                    outcome.next_state = Some(TextSearchCursorModeV1::Corpus {
                        file_index,
                        chunk_index,
                        line_offset: offset,
                    });
                    break 'outer_corpus;
                }

                let Some(col_byte) = ContextFinderService::match_in_line(
                    line_text,
                    settings.pattern,
                    settings.case_sensitive,
                    settings.whole_word,
                ) else {
                    continue;
                };

                let line = chunk.start_line + offset;
                let column = line_text[..col_byte].chars().count() + 1;
                let mut item = TextSearchMatch {
                    file: chunk.file_path.clone(),
                    line,
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
                    outcome.next_state = Some(TextSearchCursorModeV1::Corpus {
                        file_index,
                        chunk_index,
                        line_offset: offset.saturating_add(1),
                    });
                    break 'outer_corpus;
                }

                if outcome.used_chars.saturating_add(cost) > content_max_chars {
                    outcome.truncated = true;
                    outcome.truncation = Some(BudgetTruncation::MaxChars);
                    outcome.next_state = Some(TextSearchCursorModeV1::Corpus {
                        file_index,
                        chunk_index,
                        line_offset: offset,
                    });
                    break 'outer_corpus;
                }

                if outcome.push_match(item) {
                    outcome.used_chars = outcome.used_chars.saturating_add(cost);
                }
            }
        }
    }

    Ok(outcome)
}

fn search_in_filesystem(
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

/// Bounded exact text search (literal substring), as a safe `rg` replacement.
pub(in crate::tools::dispatch) async fn text_search(
    service: &ContextFinderService,
    mut request: TextSearchRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Minimal);

    if let Some(cursor) = request.cursor.as_deref() {
        match expand_cursor_alias(service, cursor).await {
            Ok(expanded) => request.cursor = Some(expanded),
            Err(message) => {
                let meta = if response_mode == ResponseMode::Full {
                    meta_for_request(service, request.path.as_deref()).await
                } else {
                    ToolMeta::default()
                };
                return Ok(invalid_cursor_with_meta(message, meta));
            }
        }
    }

    let path_missing = match request.path.as_deref().map(str::trim) {
        Some(value) => value.is_empty(),
        None => true,
    };
    if path_missing {
        if let Some(cursor) = request
            .cursor
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Ok(decoded) = decode_cursor::<TextSearchCursorV1>(cursor) {
                if decoded.v == CURSOR_VERSION && decoded.tool == "text_search" {
                    if let Some(root) = decoded.root.as_deref().map(str::trim) {
                        if !root.is_empty() {
                            let session_root_display =
                                { service.session.lock().await.root_display.clone() };
                            if let Some(session_root_display) = session_root_display {
                                if session_root_display != root {
                                    return Ok(invalid_cursor_with_meta(
                                        "Invalid cursor: cursor refers to a different project root than the current session; pass `path` to switch projects.",
                                        ToolMeta {
                                            root_fingerprint: Some(root_fingerprint(
                                                &session_root_display,
                                            )),
                                            ..ToolMeta::default()
                                        },
                                    ));
                                }
                            }
                            request.path = Some(root.to_string());
                        }
                    }
                }
            }
        }
    }

    let mut hints: Vec<String> = Vec::new();
    if let Some(pattern) = request.file_pattern.as_deref() {
        if !pattern.trim().is_empty() {
            hints.push(pattern.to_string());
        }
    }
    let (root, root_display) = match service
        .resolve_root_with_hints_no_daemon_touch(request.path.as_deref(), &hints)
        .await
    {
        Ok(value) => value,
        Err(message) => {
            let meta = if response_mode == ResponseMode::Full {
                meta_for_request(service, request.path.as_deref()).await
            } else {
                ToolMeta::default()
            };
            return Ok(invalid_request_with_meta(message, meta, None, Vec::new()));
        }
    };
    let provenance_meta = ToolMeta {
        root_fingerprint: Some(root_fingerprint(&root_display)),
        ..ToolMeta::default()
    };
    let meta_for_output = if response_mode == ResponseMode::Full {
        service.tool_meta(&root).await
    } else {
        provenance_meta.clone()
    };

    const DEFAULT_MAX_CHARS: usize = 2_000;
    const MAX_MAX_CHARS: usize = 500_000;
    const DEFAULT_MAX_RESULTS: usize = 50;
    const MAX_MAX_RESULTS: usize = 1_000;

    let cursor_payload = match decode_cursor_payload(
        &request,
        &root_display,
        request.allow_secrets,
        &meta_for_output,
    ) {
        Ok(value) => value,
        Err(result) => return Ok(result),
    };

    let pattern = request
        .pattern
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| cursor_payload.as_ref().map(|c| c.pattern.clone()));
    let Some(pattern) = pattern else {
        return Ok(invalid_request_with_meta(
            "Pattern must not be empty (provide pattern or cursor)",
            meta_for_output.clone(),
            None,
            Vec::new(),
        ));
    };

    let normalized_file_pattern = trimmed_non_empty_str(request.file_pattern.as_deref())
        .map(str::to_string)
        .or_else(|| cursor_payload.as_ref().and_then(|c| c.file_pattern.clone()));
    let file_pattern = normalized_file_pattern.as_deref();

    let case_sensitive = request
        .case_sensitive
        .or_else(|| cursor_payload.as_ref().map(|c| c.case_sensitive))
        .unwrap_or(true);
    let whole_word = request
        .whole_word
        .or_else(|| cursor_payload.as_ref().map(|c| c.whole_word))
        .unwrap_or(false);
    let allow_secrets = request
        .allow_secrets
        .or_else(|| cursor_payload.as_ref().map(|c| c.allow_secrets))
        .unwrap_or(false);

    // Cursor safety: disallow mixing a cursor with different search options.
    if let Some(decoded) = cursor_payload.as_ref() {
        if decoded.pattern != pattern {
            return Ok(invalid_cursor_with_meta(
                "Invalid cursor: different pattern",
                meta_for_output.clone(),
            ));
        }
        if decoded.file_pattern.as_ref() != normalized_file_pattern.as_ref() {
            return Ok(invalid_cursor_with_meta(
                "Invalid cursor: different file_pattern",
                meta_for_output.clone(),
            ));
        }
        if decoded.case_sensitive != case_sensitive || decoded.whole_word != whole_word {
            return Ok(invalid_cursor_with_meta(
                "Invalid cursor: different search options",
                meta_for_output.clone(),
            ));
        }
        if decoded.allow_secrets != allow_secrets {
            return Ok(invalid_cursor_with_meta(
                "Invalid cursor: different allow_secrets",
                meta_for_output.clone(),
            ));
        }
    }

    let max_results = request
        .max_results
        .or_else(|| {
            cursor_payload
                .as_ref()
                .map(|c| c.max_results)
                .filter(|v| *v > 0)
        })
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, MAX_MAX_RESULTS);
    let max_chars = request
        .max_chars
        .or_else(|| {
            cursor_payload
                .as_ref()
                .map(|c| c.max_chars)
                .filter(|v| *v > 0)
        })
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(1, MAX_MAX_CHARS);

    let settings = TextSearchSettings {
        pattern: pattern.as_str(),
        file_pattern,
        max_results,
        max_chars,
        case_sensitive,
        whole_word,
    };
    let content_max_chars = text_search_content_budget(max_chars, response_mode);

    let corpus = match ContextFinderService::load_chunk_corpus(&root).await {
        Ok(corpus) => corpus,
        Err(err) => {
            return Ok(internal_error_with_meta(
                format!("Error: {err:#}"),
                meta_for_output.clone(),
            ))
        }
    };

    let cursor_mode = cursor_payload.as_ref().map(|payload| &payload.mode);

    // Agent-native UX: text_search should behave like a precise `rg` replacement, even when a
    // semantic corpus exists but is partial (scoped indexing, in-progress indexing, etc).
    //
    // We therefore default to filesystem scanning; corpus mode remains supported for cursor
    // continuation so older cursors keep working predictably.
    let continue_with_corpus = matches!(cursor_mode, Some(TextSearchCursorModeV1::Corpus { .. }))
        && corpus.is_some()
        && !allow_secrets;

    let (source, mut outcome) = if continue_with_corpus {
        let corpus = corpus.expect("corpus checked above");
        let (start_file_index, start_chunk_index, start_line_offset) =
            match start_indices_for_corpus(cursor_mode) {
                Ok(value) => value,
                Err(result) => {
                    return Ok(attach_meta(result, meta_for_output.clone()));
                }
            };
        let outcome = match search_in_corpus(
            &corpus,
            &settings,
            content_max_chars,
            start_file_index,
            start_chunk_index,
            start_line_offset,
        ) {
            Ok(value) => value,
            Err(result) => {
                return Ok(attach_meta(result, meta_for_output.clone()));
            }
        };
        ("corpus".to_string(), outcome)
    } else {
        let (start_file_index, start_line_offset) = match start_indices_for_filesystem(cursor_mode)
        {
            Ok(value) => value,
            Err(result) => {
                return Ok(attach_meta(result, meta_for_output.clone()));
            }
        };
        let outcome = match search_in_filesystem(
            &root,
            &settings,
            allow_secrets,
            content_max_chars,
            start_file_index,
            start_line_offset,
        ) {
            Ok(value) => value,
            Err(result) => {
                return Ok(attach_meta(result, meta_for_output.clone()));
            }
        };
        ("filesystem".to_string(), outcome)
    };

    let next_cursor = if outcome.truncated {
        let Some(mode) = outcome.next_state.take() else {
            return Ok(internal_error("Internal error: missing cursor state"));
        };
        match encode_next_cursor(
            &root_display,
            &settings,
            normalized_file_pattern.as_ref(),
            allow_secrets,
            mode,
        ) {
            Ok(value) => Some(value),
            Err(result) => {
                return Ok(attach_meta(result, meta_for_output.clone()));
            }
        }
    } else {
        None
    };

    let mut result = TextSearchResult {
        pattern: settings.pattern.to_string(),
        source: Some(source),
        scanned_files: Some(outcome.scanned_files),
        matched_files: Some(outcome.matched_files.len()),
        skipped_large_files: Some(outcome.skipped_large_files),
        returned: Some(outcome.matches.len()),
        truncated: outcome.truncated,
        truncation: outcome.truncation,
        used_chars: Some(0),
        max_chars: Some(max_chars),
        next_cursor,
        next_actions: None,
        meta: Some(ToolMeta::default()),
        matches: outcome.matches,
    };
    match response_mode {
        ResponseMode::Full => {
            result.meta = Some(meta_for_output.clone());
        }
        ResponseMode::Facts => {
            result.meta = Some(provenance_meta.clone());
        }
        ResponseMode::Minimal => {
            result.meta = Some(provenance_meta.clone());
            result.source = None;
            result.scanned_files = None;
            result.matched_files = None;
            result.skipped_large_files = None;
            result.returned = None;
            result.truncation = None;
            result.used_chars = None;
            result.max_chars = None;
        }
    }
    if let Some(cursor) = result.next_cursor.take() {
        result.next_cursor = Some(compact_cursor_alias(service, cursor).await);
    }
    if response_mode == ResponseMode::Full {
        if let Some(cursor) = result.next_cursor.clone() {
            result.next_actions = Some(vec![ToolNextAction {
                tool: "text_search".to_string(),
                args: json!({
                    "path": root_display,
                    "cursor": cursor,
                }),
                reason: "Continue text_search pagination with the next cursor.".to_string(),
            }]);
        }
    }

    fn shrink_last_match_text(result: &mut TextSearchResult) -> bool {
        let Some(last) = result.matches.last_mut() else {
            return false;
        };
        if last.text.is_empty() {
            return false;
        }
        let cur_chars = last.text.chars().count();
        if cur_chars <= 1 {
            last.text.clear();
            return true;
        }
        let new_chars = cur_chars.div_ceil(2);
        last.text = truncate_to_chars(&last.text, new_chars);
        true
    }

    loop {
        let mut doc = ContextDocBuilder::new();
        doc.push_answer(&format!(
            "Matches: {} (pattern={})",
            result.matches.len(),
            result.pattern
        ));
        if response_mode != ResponseMode::Minimal {
            doc.push_root_fingerprint(meta_for_output.root_fingerprint);
        }
        if result.matches.is_empty() && !result.truncated {
            doc.push_note("hint: no matches");
            doc.push_note("next: rg (regex + context)");
        }

        // Agent-native packing: group matches by file so the output is mostly project payload,
        // not repeated `R:` headers. Preserve the first-seen file order for determinism.
        let mut groups: Vec<(String, Vec<&TextSearchMatch>)> = Vec::new();
        let mut group_lookup: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for item in &result.matches {
            let idx = match group_lookup.get(item.file.as_str()) {
                Some(&idx) => idx,
                None => {
                    let idx = groups.len();
                    groups.push((item.file.clone(), Vec::new()));
                    group_lookup.insert(item.file.as_str(), idx);
                    idx
                }
            };
            groups[idx].1.push(item);
        }

        for (file, matches) in groups {
            let start_line = matches.first().map(|m| m.line).unwrap_or(1);
            if response_mode == ResponseMode::Minimal {
                doc.push_line(&format!("-- {file} --"));
            } else {
                doc.push_ref_header(&file, start_line, Some("matches"));
            }
            for m in matches {
                doc.push_line(&format!("{}:{}: {}", m.line, m.column, m.text));
            }
            doc.push_blank();
        }
        if result.truncated {
            if let Some(cursor) = result.next_cursor.as_deref() {
                doc.push_cursor(cursor);
            }
        }
        let raw = doc.finish();
        let used = raw.chars().count();
        if let Some(slot) = result.used_chars.as_mut() {
            *slot = used;
        }
        if used <= max_chars {
            let output = CallToolResult::success(vec![Content::text(raw)]);
            return Ok(attach_structured_content(
                output,
                &result,
                meta_for_output.clone(),
                "text_search",
            ));
        }

        result.truncated = true;
        if shrink_last_match_text(&mut result) {
            continue;
        }

        // Fail-soft: under extremely small budgets the envelope can dominate; return a bounded
        // `.context` payload instead of erroring.
        let bounded = crate::tools::util::truncate_to_chars(&raw, max_chars);
        let output = CallToolResult::success(vec![Content::text(bounded)]);
        return Ok(attach_structured_content(
            output,
            &result,
            meta_for_output,
            "text_search",
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::{TextSearchMatch, TextSearchOutcome};

    #[test]
    fn text_search_dedupes_matches() {
        let mut outcome = TextSearchOutcome::new();
        let first = TextSearchMatch {
            file: "src/main.rs".to_string(),
            line: 1,
            column: 1,
            text: "fn main() {}".to_string(),
        };
        assert!(outcome.push_match(first));

        let dup = TextSearchMatch {
            file: "src/main.rs".to_string(),
            line: 1,
            column: 1,
            text: "fn main() {}".to_string(),
        };
        assert!(!outcome.push_match(dup));
        assert_eq!(outcome.matches.len(), 1);
        assert_eq!(outcome.matched_files.len(), 1);
    }
}
