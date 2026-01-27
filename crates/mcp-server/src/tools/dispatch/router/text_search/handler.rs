use super::budget::text_search_content_budget;
use super::cursor::{
    decode_cursor_payload, encode_next_cursor, start_indices_for_corpus,
    start_indices_for_filesystem, validate_cursor_matches_settings,
};
use super::scan_corpus::search_in_corpus;
use super::scan_filesystem::search_in_filesystem;
use super::types::TextSearchSettings;

use crate::tools::cursor::{decode_cursor, CURSOR_VERSION};
use crate::tools::dispatch::{CallToolResult, Content, ContextFinderService, McpError, ToolMeta};
use crate::tools::schemas::response_mode::ResponseMode;
use crate::tools::schemas::text_search::{
    TextSearchCursorModeV1, TextSearchCursorV1, TextSearchMatch, TextSearchRequest,
    TextSearchResult,
};

use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::schemas::ToolNextAction;
use context_indexer::root_fingerprint;
use context_protocol::BudgetTruncation;
use serde_json::json;
use std::path::Path;

use super::super::cursor_alias::{compact_cursor_alias, expand_cursor_alias};
use super::super::error::{
    attach_meta, attach_structured_content, internal_error, internal_error_with_meta,
    invalid_cursor_with_meta, invalid_request_with_meta, invalid_request_with_root_context,
    meta_for_request,
};

fn trimmed_non_empty_str(input: Option<&str>) -> Option<&str> {
    input.map(str::trim).filter(|value| !value.is_empty())
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
    last.text = crate::tools::util::truncate_to_chars(&last.text, new_chars);
    true
}

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
                                { service.session.lock().await.root_display() };
                            if let Some(session_root_display) = session_root_display {
                                if session_root_display != root {
                                    return Ok(invalid_cursor_with_meta(
                                        "Invalid cursor: cursor refers to a different project root than the current session; call `root_set` to switch projects (or pass `path`).",
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

    // DX convenience: callers often pass `path` as a *subdirectory within the project* (e.g.
    // `{ "path": "src", "pattern": "TODO" }`) expecting the tool to be scoped. In Context, `path`
    // sets the project root; scoping should use `file_pattern`.
    //
    // When the session already has a root, treat a relative `path` with no `file_pattern` and no
    // cursor as a `file_pattern` hint instead of switching the session root.
    let cursor_missing = request
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none();
    let file_pattern_missing = trimmed_non_empty_str(request.file_pattern.as_deref()).is_none();
    if cursor_missing && file_pattern_missing {
        if let Some(raw_path) =
            trimmed_non_empty_str(request.path.as_deref()).filter(|s| *s != "." && *s != "./")
        {
            let session_root = { service.session.lock().await.clone_root().map(|(r, _)| r) };
            if let Some(session_root) = session_root.as_ref() {
                let raw = Path::new(raw_path);
                if raw.is_absolute() {
                    if let Ok(canonical) = raw.canonicalize() {
                        if canonical.starts_with(session_root) {
                            if let Ok(rel) = canonical.strip_prefix(session_root) {
                                if let Some(rel) =
                                    crate::tools::dispatch::root::rel_path_string(rel)
                                {
                                    let mut pattern = rel;
                                    let is_dir = std::fs::metadata(&canonical)
                                        .ok()
                                        .map(|meta| meta.is_dir())
                                        .unwrap_or(false);
                                    if is_dir && !pattern.ends_with('/') {
                                        pattern.push('/');
                                    }
                                    request.file_pattern = Some(pattern);
                                    request.path = None;
                                }
                            }
                        }
                    }
                } else {
                    let candidate = session_root.join(raw_path);
                    let is_dir = std::fs::metadata(&candidate)
                        .ok()
                        .map(|meta| meta.is_dir())
                        .unwrap_or(false);
                    let mut pattern = raw_path.to_string();
                    if is_dir && !pattern.ends_with('/') {
                        pattern.push('/');
                    }
                    request.file_pattern = Some(pattern);
                    request.path = None;
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
        .resolve_root_with_hints_no_daemon_touch_for_tool(
            request.path.as_deref(),
            &hints,
            "text_search",
        )
        .await
    {
        Ok(value) => value,
        Err(message) => {
            let meta = if response_mode == ResponseMode::Full {
                meta_for_request(service, request.path.as_deref()).await
            } else {
                ToolMeta::default()
            };
            return Ok(
                invalid_request_with_root_context(service, message, meta, None, Vec::new()).await,
            );
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

    // Cursor safety: disallow mixing a cursor with different search options.
    if let Some(decoded) = cursor_payload.as_ref() {
        if let Err(result) = validate_cursor_matches_settings(
            decoded,
            &root_display,
            &settings,
            normalized_file_pattern.as_ref(),
            allow_secrets,
        ) {
            return Ok(attach_meta(result, meta_for_output.clone()));
        }
    }
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
        result.truncation = Some(BudgetTruncation::MaxChars);

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
