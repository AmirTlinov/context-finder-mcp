use super::super::{
    compute_grep_context_result, decode_cursor, CallToolResult, Content, ContextFinderService,
    GrepContextComputeOptions, GrepContextCursorV1, GrepContextRequest, McpError, ResponseMode,
    ToolMeta, CURSOR_VERSION,
};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::cursor::{cursor_fingerprint, encode_cursor};
use crate::tools::schemas::content_format::ContentFormat;
use crate::tools::schemas::grep_context::{GrepContextResult, GrepContextTruncation};
use crate::tools::schemas::ToolNextAction;
use crate::tools::secrets::is_potential_secret_path;
use context_indexer::root_fingerprint;
use regex::RegexBuilder;
use serde_json::json;

use super::cursor_alias::{compact_cursor_alias, expand_cursor_alias};
use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_cursor_with_meta,
    invalid_cursor_with_meta_details, invalid_request_with_meta, meta_for_request,
};

fn build_regex(pattern: &str, case_sensitive: bool) -> Result<regex::Regex, String> {
    RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|err| format!("Invalid regex: {err}"))
}

pub(super) fn grep_context_content_budget(max_chars: usize, response_mode: ResponseMode) -> usize {
    const MIN_CONTENT_CHARS: usize = 120;
    const MAX_RESERVE_CHARS: usize = 4_096;

    // `.context` envelopes are small. Keep the reserve low so budgets are spent on hunk payload.
    let base_reserve = match response_mode {
        ResponseMode::Minimal => 120,
        ResponseMode::Facts => 180,
        ResponseMode::Full => 520,
    };

    let proportional = max_chars / 20;
    let mut reserve = base_reserve.max(proportional).min(MAX_RESERVE_CHARS);
    reserve = reserve.min(max_chars.saturating_sub(MIN_CONTENT_CHARS));
    max_chars.saturating_sub(reserve).max(1)
}

fn render_grep_context_context_doc(
    result: &GrepContextResult,
    _response_mode: ResponseMode,
) -> String {
    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!(
        "Matches: {} (pattern={})",
        result.hunks.len(),
        result.pattern
    ));
    doc.push_root_fingerprint(result.meta.as_ref().and_then(|meta| meta.root_fingerprint));
    for hunk in &result.hunks {
        doc.push_ref_header(&hunk.file, hunk.start_line, Some("grep hunk"));
        doc.push_block_smart(&hunk.content);
        doc.push_blank();
    }
    if result.truncated {
        if let Some(cursor) = result.next_cursor.as_deref() {
            doc.push_cursor(cursor);
        }
    }
    doc.finish()
}

fn finalize_grep_context_budget_context(
    result: &mut GrepContextResult,
    max_chars: usize,
    response_mode: ResponseMode,
) -> anyhow::Result<bool> {
    fn recompute_returned_matches(result: &mut GrepContextResult) {
        if let Some(returned_matches) = result.returned_matches.as_mut() {
            *returned_matches = result
                .hunks
                .iter()
                .map(|hunk| hunk.match_lines.as_ref().map_or(0, |v| v.len()))
                .sum();
        }
        if let Some(returned_hunks) = result.returned_hunks.as_mut() {
            *returned_hunks = result.hunks.len();
        }
    }

    fn shrink_last_hunk(result: &mut GrepContextResult) -> bool {
        let Some(last) = result.hunks.last_mut() else {
            return false;
        };
        if last.content.is_empty() {
            return false;
        }

        // Prefer to drop whole lines (keeps start/end_line consistent with content).
        let total_lines = last.content.lines().count();
        if total_lines > 1 {
            let keep_lines = total_lines.div_ceil(2);

            // If we know match line(s), keep a window that includes at least one match.
            if let Some(match_lines) = last.match_lines.as_ref() {
                if let Some(&anchor_line) = match_lines.iter().min() {
                    if anchor_line >= last.start_line && anchor_line <= last.end_line {
                        let anchor_idx = anchor_line.saturating_sub(last.start_line);
                        let half = keep_lines / 2;
                        let mut window_start = anchor_idx.saturating_sub(half);
                        if window_start.saturating_add(keep_lines) > total_lines {
                            window_start = total_lines.saturating_sub(keep_lines);
                        }

                        let lines: Vec<&str> = last.content.lines().collect();
                        let window_end = (window_start + keep_lines).min(lines.len());
                        let kept = lines[window_start..window_end].join("\n");

                        last.content = kept;
                        last.start_line = last.start_line.saturating_add(window_start);
                        let kept_lines = last.content.lines().count().max(1);
                        last.end_line =
                            last.start_line.saturating_add(kept_lines.saturating_sub(1));
                        if let Some(match_lines) = last.match_lines.as_mut() {
                            match_lines.retain(|&ln| ln >= last.start_line && ln <= last.end_line);
                        }
                        if last
                            .match_lines
                            .as_ref()
                            .is_some_and(|match_lines| match_lines.is_empty())
                        {
                            last.match_lines = None;
                        }
                        recompute_returned_matches(result);
                        return true;
                    }
                }
            }

            // Fallback: keep the first half (deterministic).
            let mut newline_count = 0usize;
            let mut cut_idx = None;
            for (idx, b) in last.content.as_bytes().iter().enumerate() {
                if *b == b'\n' {
                    newline_count += 1;
                    if newline_count == keep_lines {
                        cut_idx = Some(idx);
                        break;
                    }
                }
            }

            if let Some(idx) = cut_idx {
                last.content.truncate(idx);
                let kept_lines = last.content.lines().count().max(1);
                last.end_line = last.start_line.saturating_add(kept_lines.saturating_sub(1));
                if let Some(match_lines) = last.match_lines.as_mut() {
                    match_lines.retain(|&ln| ln <= last.end_line);
                }
                if last
                    .match_lines
                    .as_ref()
                    .is_some_and(|match_lines| match_lines.is_empty())
                {
                    last.match_lines = None;
                }
                recompute_returned_matches(result);
                return true;
            }
        }

        // Single-line hunk: fall back to character truncation.
        let cur_chars = last.content.chars().count();
        if cur_chars <= 1 {
            return false;
        }
        let new_chars = cur_chars.div_ceil(2);
        let mut cut_byte = last.content.len();
        for (seen, (idx, _)) in last.content.char_indices().enumerate() {
            if seen == new_chars {
                cut_byte = idx;
                break;
            }
        }
        if cut_byte == 0 {
            return false;
        }
        last.content.truncate(cut_byte);
        last.end_line = last.start_line;
        if let Some(match_lines) = last.match_lines.as_mut() {
            match_lines.retain(|&ln| ln == last.start_line);
        }
        if last
            .match_lines
            .as_ref()
            .is_some_and(|match_lines| match_lines.is_empty())
        {
            last.match_lines = None;
        }
        recompute_returned_matches(result);
        true
    }

    let mut trimmed = false;
    loop {
        let raw = render_grep_context_context_doc(result, response_mode);
        let used = raw.chars().count();
        if used <= max_chars {
            if let Some(slot) = result.used_chars.as_mut() {
                *slot = used;
            }
            return Ok(trimmed);
        }

        result.truncated = true;
        result.truncation = Some(GrepContextTruncation::MaxChars);

        if shrink_last_hunk(result) {
            trimmed = true;
            continue;
        }
        if !result.hunks.is_empty() {
            result.hunks.pop();
            recompute_returned_matches(result);
            trimmed = true;
            continue;
        }

        anyhow::bail!("budget exceeded (used_chars={used}, max_chars={max_chars})");
    }
}

struct CursorValidation<'a> {
    root_display: &'a str,
    root_hash: u64,
    pattern: &'a str,
    literal: bool,
    case_sensitive: bool,
    before: usize,
    after: usize,
    normalized_file: Option<&'a str>,
    normalized_file_pattern: Option<&'a str>,
    allow_secrets: bool,
}

fn decode_resume_cursor(
    cursor: Option<&str>,
    validation: &CursorValidation<'_>,
) -> Result<(Option<String>, usize), String> {
    let Some(cursor) = cursor else {
        return Ok((None, 1));
    };
    let cursor = cursor.trim();
    if cursor.is_empty() {
        return Ok((None, 1));
    }

    let decoded: GrepContextCursorV1 =
        decode_cursor(cursor).map_err(|err| format!("Invalid cursor: {err}"))?;
    if decoded.v != CURSOR_VERSION || (decoded.tool != "rg" && decoded.tool != "grep_context") {
        return Err("Invalid cursor: wrong tool".to_string());
    }
    if let Some(hash) = decoded.root_hash {
        if hash != validation.root_hash {
            return Err("Invalid cursor: different root".to_string());
        }
    } else if decoded.root.as_deref() != Some(validation.root_display) {
        return Err("Invalid cursor: different root".to_string());
    }
    if decoded.pattern != validation.pattern {
        return Err("Invalid cursor: different pattern".to_string());
    }
    if decoded.literal != validation.literal {
        return Err("Invalid cursor: different literal mode".to_string());
    }
    if decoded.file.as_deref() != validation.normalized_file {
        return Err("Invalid cursor: different file".to_string());
    }
    if decoded.file_pattern.as_deref() != validation.normalized_file_pattern {
        return Err("Invalid cursor: different file_pattern".to_string());
    }
    if decoded.case_sensitive != validation.case_sensitive
        || decoded.before != validation.before
        || decoded.after != validation.after
    {
        return Err("Invalid cursor: different search options".to_string());
    }
    if decoded.allow_secrets != validation.allow_secrets {
        return Err("Invalid cursor: different allow_secrets".to_string());
    }
    Ok((Some(decoded.resume_file), decoded.resume_line.max(1)))
}

/// Regex search with merged context hunks (grep-like).
pub(in crate::tools::dispatch) async fn grep_context(
    service: &ContextFinderService,
    mut request: GrepContextRequest,
) -> Result<CallToolResult, McpError> {
    const DEFAULT_MAX_CHARS: usize = 2_000;
    const MAX_MAX_CHARS: usize = 500_000;
    const DEFAULT_MAX_MATCHES: usize = 2_000;
    const MAX_MAX_MATCHES: usize = 50_000;
    const DEFAULT_MAX_HUNKS: usize = 200;
    const MAX_MAX_HUNKS: usize = 50_000;
    const DEFAULT_CONTEXT: usize = 20;
    const MAX_CONTEXT: usize = 5_000;

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
            if let Ok(decoded) = decode_cursor::<GrepContextCursorV1>(cursor) {
                if decoded.v == CURSOR_VERSION
                    && (decoded.tool == "rg" || decoded.tool == "grep_context")
                {
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
    if let Some(file) = request.file.as_deref() {
        hints.push(file.to_string());
    }
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

    let cursor_payload: Option<GrepContextCursorV1> = match request
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(cursor) => match decode_cursor(cursor) {
            Ok(decoded) => Some(decoded),
            Err(err) => {
                return Ok(invalid_cursor_with_meta(
                    format!("Invalid cursor: {err}"),
                    meta_for_output.clone(),
                ))
            }
        },
        None => None,
    };

    if let Some(decoded) = cursor_payload.as_ref() {
        if decoded.v != CURSOR_VERSION || (decoded.tool != "rg" && decoded.tool != "grep_context") {
            return Ok(invalid_cursor_with_meta(
                "Invalid cursor: wrong tool",
                meta_for_output.clone(),
            ));
        }
        if let Some(hash) = decoded.root_hash {
            if hash != cursor_fingerprint(&root_display) {
                let expected_root_fingerprint = meta_for_output
                    .root_fingerprint
                    .unwrap_or_else(|| root_fingerprint(&root_display));
                return Ok(invalid_cursor_with_meta_details(
                    "Invalid cursor: different root",
                    meta_for_output.clone(),
                    json!({
                        "expected_root_fingerprint": expected_root_fingerprint,
                        "cursor_root_fingerprint": Some(hash),
                    }),
                ));
            }
        } else if decoded.root.as_deref() != Some(&root_display) {
            let expected_root_fingerprint = meta_for_output
                .root_fingerprint
                .unwrap_or_else(|| root_fingerprint(&root_display));
            let cursor_root_fingerprint = decoded.root.as_deref().map(root_fingerprint);
            return Ok(invalid_cursor_with_meta_details(
                "Invalid cursor: different root",
                meta_for_output.clone(),
                json!({
                    "expected_root_fingerprint": expected_root_fingerprint,
                    "cursor_root_fingerprint": cursor_root_fingerprint,
                }),
            ));
        }
    }

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

    let literal = request
        .literal
        .or_else(|| cursor_payload.as_ref().map(|c| c.literal))
        .unwrap_or(false);
    let case_sensitive = request
        .case_sensitive
        .or_else(|| cursor_payload.as_ref().map(|c| c.case_sensitive))
        .unwrap_or(true);
    let regex_pattern = if literal {
        regex::escape(&pattern)
    } else {
        pattern.clone()
    };
    let regex = match build_regex(&regex_pattern, case_sensitive) {
        Ok(re) => re,
        Err(msg) => {
            let (hint, next_actions) = if response_mode == ResponseMode::Full {
                (
                    Some(
                        "If you intended a literal match, set `literal: true`. When calling via JSON, remember to escape backslashes (e.g. use `\\\\(` to match a literal `(`).".to_string(),
                    ),
                    vec![
                        ToolNextAction {
                            tool: "rg".to_string(),
                            args: json!({
                                "path": root_display.clone(),
                                "pattern": pattern.clone(),
                                "literal": true,
                                "case_sensitive": case_sensitive,
                                "context": request.context.unwrap_or(DEFAULT_CONTEXT),
                                "max_chars": request.max_chars.unwrap_or(2_000),
                                "max_hunks": request.max_hunks.unwrap_or(8),
                                "format": "numbered",
                                "response_mode": "facts"
                            }),
                            reason: "Retry rg with literal=true (no regex parsing)."
                                .to_string(),
                        },
                        ToolNextAction {
                            tool: "text_search".to_string(),
                            args: json!({
                                "path": root_display.clone(),
                                "pattern": pattern.clone(),
                                "max_results": 80,
                                "case_sensitive": case_sensitive,
                                "whole_word": false,
                                "response_mode": "facts"
                            }),
                            reason: "If the term is short, text_search may be a faster/simpler fallback than regex."
                                .to_string(),
                        },
                    ],
                )
            } else {
                (None, Vec::new())
            };
            return Ok(invalid_request_with_meta(
                msg,
                meta_for_output.clone(),
                hint,
                next_actions,
            ));
        }
    };

    let before = request
        .before
        .or(request.context)
        .or_else(|| cursor_payload.as_ref().map(|c| c.before))
        .unwrap_or(DEFAULT_CONTEXT)
        .clamp(0, MAX_CONTEXT);
    let after = request
        .after
        .or(request.context)
        .or_else(|| cursor_payload.as_ref().map(|c| c.after))
        .unwrap_or(DEFAULT_CONTEXT)
        .clamp(0, MAX_CONTEXT);

    let max_matches = request
        .max_matches
        .or_else(|| {
            cursor_payload
                .as_ref()
                .and_then(|c| (c.max_matches > 0).then_some(c.max_matches))
        })
        .unwrap_or(DEFAULT_MAX_MATCHES)
        .clamp(1, MAX_MAX_MATCHES);
    let max_hunks = request
        .max_hunks
        .or_else(|| {
            cursor_payload
                .as_ref()
                .and_then(|c| (c.max_hunks > 0).then_some(c.max_hunks))
        })
        .unwrap_or(DEFAULT_MAX_HUNKS)
        .clamp(1, MAX_MAX_HUNKS);
    let max_chars = request
        .max_chars
        .or_else(|| {
            cursor_payload
                .as_ref()
                .and_then(|c| (c.max_chars > 0).then_some(c.max_chars))
        })
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(1, MAX_MAX_CHARS);
    let content_max_chars = grep_context_content_budget(max_chars, response_mode);

    let normalized_file = request
        .file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let normalized_file_pattern = request
        .file_pattern
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let normalized_file =
        normalized_file.or_else(|| cursor_payload.as_ref().and_then(|c| c.file.clone()));
    let normalized_file_pattern = normalized_file_pattern
        .or_else(|| cursor_payload.as_ref().and_then(|c| c.file_pattern.clone()));

    let allow_secrets = request
        .allow_secrets
        .or_else(|| cursor_payload.as_ref().map(|c| c.allow_secrets))
        .unwrap_or(false);
    if !allow_secrets {
        if let Some(file) = normalized_file.as_deref() {
            if is_potential_secret_path(file) {
                return Ok(invalid_request_with_meta(
                    "Refusing to search potential secret file (set allow_secrets=true to override)",
                    meta_for_output.clone(),
                    None,
                    Vec::new(),
                ));
            }
        }
    }

    request.pattern = Some(pattern.clone());
    request.literal = Some(literal);
    request.file = normalized_file.clone();
    request.file_pattern = normalized_file_pattern.clone();
    request.allow_secrets = Some(allow_secrets);
    if request.format.is_none() {
        request.format = cursor_payload.as_ref().map(|c| c.format);
    }
    // Low-noise default: when the agent isn't explicitly asking for debug-rich output, keep grep
    // hunks "plain" (no per-line number prefixes) and rely on structured line ranges instead.
    // This significantly increases payload density under max_chars budgets.
    if request.format.is_none()
        && matches!(response_mode, ResponseMode::Facts | ResponseMode::Minimal)
    {
        request.format = Some(ContentFormat::Plain);
    }

    let (resume_file, resume_line) = match decode_resume_cursor(
        request.cursor.as_deref(),
        &CursorValidation {
            root_display: &root_display,
            root_hash: cursor_fingerprint(&root_display),
            pattern: &pattern,
            literal,
            case_sensitive,
            before,
            after,
            normalized_file: normalized_file.as_deref(),
            normalized_file_pattern: normalized_file_pattern.as_deref(),
            allow_secrets,
        },
    ) {
        Ok(v) => v,
        Err(msg) => return Ok(invalid_cursor_with_meta(msg, meta_for_output.clone())),
    };

    let mut result = match compute_grep_context_result(
        &root,
        &root_display,
        &request,
        &regex,
        GrepContextComputeOptions {
            case_sensitive,
            before,
            after,
            max_matches,
            max_hunks,
            max_chars,
            content_max_chars,
            resume_file: resume_file.as_deref(),
            resume_line,
        },
    )
    .await
    {
        Ok(result) => result,
        Err(err) => {
            return Ok(internal_error_with_meta(
                format!("Error: {err:#}"),
                meta_for_output.clone(),
            ))
        }
    };
    let raw_next_cursor = result.next_cursor.clone();
    let format = request.format.unwrap_or(ContentFormat::Numbered);
    match response_mode {
        ResponseMode::Full => {
            result.meta = Some(meta_for_output.clone());
        }
        ResponseMode::Facts => {
            result.meta = Some(provenance_meta.clone());
            // Low-noise: match line indices are helpful for debugging, but are usually redundant
            // for agents once the surrounding hunk content is present.
            for hunk in &mut result.hunks {
                hunk.match_lines = None;
            }
        }
        ResponseMode::Minimal => {
            result.meta = Some(provenance_meta.clone());
            result.source = None;
            result.file = None;
            result.file_pattern = None;
            result.case_sensitive = None;
            result.before = None;
            result.after = None;
            result.scanned_files = None;
            result.matched_files = None;
            result.returned_matches = None;
            result.returned_hunks = None;
            result.used_chars = None;
            result.max_chars = None;
            for hunk in &mut result.hunks {
                hunk.match_lines = None;
            }
        }
    }
    if let Some(cursor) = result.next_cursor.take() {
        result.next_cursor = Some(compact_cursor_alias(service, cursor).await);
    }
    let finalize_res = finalize_grep_context_budget_context(&mut result, max_chars, response_mode);
    let trimmed = match finalize_res {
        Ok(trimmed) => trimmed,
        Err(_err) => {
            // Fail-soft: never error solely due to budget envelope constraints.
            result.truncated = true;
            result.truncation = Some(GrepContextTruncation::MaxChars);
            result.hunks.clear();
            false
        }
    };

    // If budget trimming modified the last hunk, the cursor computed during hunk assembly can now
    // skip lines that were dropped. Re-anchor the cursor to the end of the last returned hunk.
    if trimmed {
        if let (Some(raw_cursor), Some(last_hunk)) =
            (raw_next_cursor.as_deref(), result.hunks.last())
        {
            if let Ok(mut decoded) = decode_cursor::<GrepContextCursorV1>(raw_cursor) {
                decoded.resume_file = last_hunk.file.clone();
                decoded.resume_line = last_hunk.end_line.saturating_add(1).max(1);
                if let Ok(updated_raw) = encode_cursor(&decoded) {
                    result.next_cursor = Some(compact_cursor_alias(service, updated_raw).await);
                }
            }
        }
    }

    // Under very tight `max_chars`, the hunk assembler can finish without ever hitting
    // `content_max_chars`, and we can still end up truncating during final JSON budgeting due to
    // envelope overhead. In that case we must still return a cursor (cursor-first contract),
    // otherwise the agent can't continue pagination.
    if result.truncated && result.next_cursor.is_none() {
        if let Some(last_hunk) = result.hunks.last() {
            let synthesized = GrepContextCursorV1 {
                v: CURSOR_VERSION,
                tool: "rg".to_string(),
                root: Some(root_display.clone()),
                root_hash: Some(cursor_fingerprint(&root_display)),
                pattern: pattern.clone(),
                file: request.file.clone(),
                file_pattern: request.file_pattern.clone(),
                literal,
                case_sensitive,
                before,
                after,
                max_matches,
                max_hunks,
                max_chars,
                format,
                allow_secrets,
                resume_file: last_hunk.file.clone(),
                resume_line: last_hunk.end_line.saturating_add(1).max(1),
            };

            if let Ok(encoded) = encode_cursor(&synthesized) {
                result.next_cursor = Some(compact_cursor_alias(service, encoded).await);

                // Adding the cursor can push us back over budget; re-finalize and then re-anchor.
                match finalize_grep_context_budget_context(&mut result, max_chars, response_mode) {
                    Ok(_more_trimmed) => {
                        if let Some(last_hunk) = result.hunks.last() {
                            let mut anchored = synthesized;
                            anchored.resume_file = last_hunk.file.clone();
                            anchored.resume_line = last_hunk.end_line.saturating_add(1).max(1);
                            if let Ok(updated_raw) = encode_cursor(&anchored) {
                                result.next_cursor =
                                    Some(compact_cursor_alias(service, updated_raw).await);
                            }
                        }
                    }
                    Err(_err) => {
                        // Fail-soft: keep the tool usable even when the requested budget cannot fit
                        // the envelope + cursor. We still return a bounded `.context` payload.
                        result.truncated = true;
                        result.truncation = Some(GrepContextTruncation::MaxChars);
                        result.hunks.clear();
                    }
                }
            }
        }
    }

    if response_mode == ResponseMode::Full {
        if let Some(cursor) = result.next_cursor.clone() {
            result.next_actions = Some(vec![ToolNextAction {
                tool: "rg".to_string(),
                args: json!({
                    "path": root_display,
                    "cursor": cursor,
                }),
                reason: "Continue rg pagination with the next cursor.".to_string(),
            }]);
        }
    }
    if trimmed && result.next_cursor.is_none() {
        // Defensive: never drop the cursor after trimming content; it is the continuation contract.
        if let Some(raw_cursor) = raw_next_cursor {
            result.next_cursor = Some(compact_cursor_alias(service, raw_cursor).await);
        }
    }

    if let Err(err) = context_protocol::serialize_json(&result) {
        return Ok(invalid_request_with_meta(
            format!("failed to serialize response ({err:#})"),
            meta_for_output,
            None,
            Vec::new(),
        ));
    }

    let mut rendered = render_grep_context_context_doc(&result, response_mode);
    if rendered.chars().count() > max_chars {
        rendered = crate::tools::util::truncate_to_chars(&rendered, max_chars);
    }
    let output = CallToolResult::success(vec![Content::text(rendered)]);
    Ok(attach_structured_content(
        output,
        &result,
        meta_for_output,
        "grep_context",
    ))
}
