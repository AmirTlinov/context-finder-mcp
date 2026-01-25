use super::super::router::error::invalid_cursor_with_meta_details;
use super::super::{decode_cursor, GrepContextCursorV1};
use super::cursors::trimmed_non_empty_str;
use super::{call_error, ReadPackContext, ToolResult, CURSOR_VERSION};
use crate::tools::cursor::cursor_fingerprint;
use context_indexer::{root_fingerprint, ToolMeta};
use serde_json::json;

pub(super) fn decode_grep_cursor(cursor: Option<&str>) -> ToolResult<Option<GrepContextCursorV1>> {
    let Some(cursor) = trimmed_non_empty_str(cursor) else {
        return Ok(None);
    };

    let decoded: GrepContextCursorV1 = decode_cursor(cursor)
        .map_err(|err| call_error("invalid_cursor", format!("Invalid cursor: {err}")))?;
    Ok(Some(decoded))
}

pub(super) fn resolve_grep_pattern(
    request_pattern: Option<&str>,
    cursor_payload: Option<&GrepContextCursorV1>,
    ctx: &ReadPackContext,
) -> ToolResult<String> {
    if let Some(pattern) = trimmed_non_empty_str(request_pattern) {
        return Ok(pattern.to_string());
    }

    if let Some(decoded) = cursor_payload {
        validate_grep_cursor_tool_root(decoded, &ctx.root_display)?;
        return Ok(decoded.pattern.clone());
    }

    Err(call_error(
        "missing_field",
        "Error: pattern is required for intent=grep",
    ))
}

pub(super) struct GrepResumeCheck<'a> {
    pub pattern: &'a str,
    pub file: Option<&'a String>,
    pub file_pattern: Option<&'a String>,
    pub case_sensitive: bool,
    pub before: usize,
    pub after: usize,
    pub allow_secrets: bool,
}

pub(super) fn resolve_grep_resume(
    cursor_payload: Option<&GrepContextCursorV1>,
    ctx: &ReadPackContext,
    check: &GrepResumeCheck<'_>,
) -> ToolResult<(Option<String>, usize)> {
    let Some(decoded) = cursor_payload else {
        return Ok((None, 1));
    };
    validate_grep_cursor_tool_root(decoded, &ctx.root_display)?;

    if decoded.pattern != check.pattern {
        return Err(call_error(
            "invalid_cursor",
            "Invalid cursor: different pattern",
        ));
    }
    if decoded.file.as_ref() != check.file {
        return Err(call_error(
            "invalid_cursor",
            "Invalid cursor: different file",
        ));
    }
    if decoded.file_pattern.as_ref() != check.file_pattern {
        return Err(call_error(
            "invalid_cursor",
            "Invalid cursor: different file_pattern",
        ));
    }
    if decoded.case_sensitive != check.case_sensitive
        || decoded.before != check.before
        || decoded.after != check.after
    {
        return Err(call_error(
            "invalid_cursor",
            "Invalid cursor: different search options",
        ));
    }
    if decoded.allow_secrets != check.allow_secrets {
        return Err(call_error(
            "invalid_cursor",
            "Invalid cursor: different allow_secrets",
        ));
    }

    Ok((
        Some(decoded.resume_file.clone()),
        decoded.resume_line.max(1),
    ))
}

fn validate_grep_cursor_tool_root(
    decoded: &GrepContextCursorV1,
    root_display: &str,
) -> ToolResult<()> {
    if decoded.v != CURSOR_VERSION || (decoded.tool != "rg" && decoded.tool != "grep_context") {
        return Err(call_error(
            "invalid_cursor",
            "Invalid cursor: wrong tool (expected rg)",
        ));
    }
    let expected_root_hash = cursor_fingerprint(root_display);
    let expected_root_fingerprint = root_fingerprint(root_display);
    if let Some(hash) = decoded.root_hash {
        if hash != expected_root_hash {
            return Err(invalid_cursor_with_meta_details(
                "Invalid cursor: different root",
                ToolMeta {
                    root_fingerprint: Some(expected_root_fingerprint),
                    ..ToolMeta::default()
                },
                json!({
                    "expected_root_fingerprint": expected_root_fingerprint,
                    "cursor_root_fingerprint": Some(hash),
                }),
            ));
        }
    } else if decoded.root.as_deref() != Some(root_display) {
        let cursor_root_fingerprint = decoded.root.as_deref().map(root_fingerprint);
        return Err(invalid_cursor_with_meta_details(
            "Invalid cursor: different root",
            ToolMeta {
                root_fingerprint: Some(expected_root_fingerprint),
                ..ToolMeta::default()
            },
            json!({
                "expected_root_fingerprint": expected_root_fingerprint,
                "cursor_root_fingerprint": cursor_root_fingerprint,
            }),
        ));
    }
    Ok(())
}
