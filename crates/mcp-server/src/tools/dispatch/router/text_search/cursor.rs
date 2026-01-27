use crate::tools::cursor::{cursor_fingerprint, decode_cursor, encode_cursor, CURSOR_VERSION};
use crate::tools::dispatch::{CallToolResult, ToolMeta};
use crate::tools::schemas::text_search::{
    TextSearchCursorModeV1, TextSearchCursorV1, TextSearchRequest,
};
use context_indexer::root_fingerprint;
use serde_json::json;

use super::super::error::{
    internal_error, invalid_cursor, invalid_cursor_with_meta, invalid_cursor_with_meta_details,
};

use super::types::TextSearchSettings;

pub(super) fn decode_cursor_payload(
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

pub(super) fn start_indices_for_corpus(
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

pub(super) fn start_indices_for_filesystem(
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

pub(super) fn encode_next_cursor(
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

pub(super) fn validate_cursor_matches_settings(
    decoded: &TextSearchCursorV1,
    root_display: &str,
    settings: &TextSearchSettings<'_>,
    normalized_file_pattern: Option<&String>,
    allow_secrets: bool,
) -> std::result::Result<(), CallToolResult> {
    if decoded.root.as_deref() != Some(root_display) {
        return Err(invalid_cursor("Invalid cursor: different root"));
    }
    if decoded.pattern != settings.pattern {
        return Err(invalid_cursor("Invalid cursor: different pattern"));
    }
    if decoded.file_pattern.as_ref() != normalized_file_pattern {
        return Err(invalid_cursor("Invalid cursor: different file_pattern"));
    }
    if decoded.case_sensitive != settings.case_sensitive
        || decoded.whole_word != settings.whole_word
    {
        return Err(invalid_cursor("Invalid cursor: different search options"));
    }
    if decoded.allow_secrets != allow_secrets {
        return Err(invalid_cursor("Invalid cursor: different allow_secrets"));
    }
    Ok(())
}
