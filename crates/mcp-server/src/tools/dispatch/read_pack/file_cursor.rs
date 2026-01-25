use super::super::router::error::invalid_cursor_with_meta_details;
use super::super::{decode_cursor, FileSliceCursorV1};
use super::cursors::trimmed_non_empty_str;
use super::{call_error, ReadPackContext, ToolResult, CURSOR_VERSION};
use crate::tools::cursor::cursor_fingerprint;
use context_indexer::{root_fingerprint, ToolMeta};
use serde_json::json;

pub(super) fn decode_file_slice_cursor(
    cursor: Option<&str>,
) -> ToolResult<Option<FileSliceCursorV1>> {
    let Some(cursor) = trimmed_non_empty_str(cursor) else {
        return Ok(None);
    };

    let decoded: FileSliceCursorV1 = decode_cursor(cursor)
        .map_err(|err| call_error("invalid_cursor", format!("Invalid cursor: {err}")))?;
    Ok(Some(decoded))
}

pub(super) fn validate_file_slice_cursor(
    ctx: &ReadPackContext,
    decoded: &FileSliceCursorV1,
) -> ToolResult<()> {
    if decoded.v != CURSOR_VERSION || (decoded.tool != "cat" && decoded.tool != "file_slice") {
        return Err(call_error(
            "invalid_cursor",
            "Invalid cursor: wrong tool (expected cat)",
        ));
    }
    let expected_root_hash = cursor_fingerprint(&ctx.root_display);
    let expected_root_fingerprint = root_fingerprint(&ctx.root_display);
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
    } else if decoded.root.as_deref() != Some(ctx.root_display.as_str()) {
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

pub(super) fn ensure_cursor_file_matches_request(
    decoded: &FileSliceCursorV1,
    requested: &str,
) -> ToolResult<()> {
    if requested != decoded.file.as_str() {
        return Err(call_error(
            "invalid_cursor",
            format!(
                "Invalid cursor: different file (cursor={}, request={})",
                decoded.file, requested
            ),
        ));
    }
    Ok(())
}
