use super::super::decode_cursor;
use super::cursors::ReadPackRecallCursorV1;
use super::{call_error, ContextFinderService, ToolResult};
use serde_json::Value;

pub(super) async fn decode_recall_cursor(
    service: &ContextFinderService,
    cursor: &str,
) -> ToolResult<ReadPackRecallCursorV1> {
    let value: Value = decode_cursor(cursor)
        .map_err(|err| call_error("invalid_cursor", format!("Invalid cursor: {err}")))?;

    if value.get("tool").and_then(Value::as_str) != Some("read_pack")
        || value.get("mode").and_then(Value::as_str) != Some("recall")
    {
        return Err(call_error("invalid_cursor", "Invalid cursor: wrong tool"));
    }

    let store_id = value.get("store_id").and_then(|v| v.as_u64());

    if let Some(store_id) = store_id {
        let Some(bytes) = service.state.cursor_store_get(store_id).await else {
            return Err(call_error(
                "invalid_cursor",
                "Invalid cursor: expired recall continuation",
            ));
        };
        return serde_json::from_slice::<ReadPackRecallCursorV1>(&bytes).map_err(|err| {
            call_error(
                "invalid_cursor",
                format!("Invalid cursor: stored continuation decode failed: {err}"),
            )
        });
    }

    serde_json::from_value::<ReadPackRecallCursorV1>(value).map_err(|err| {
        call_error(
            "invalid_cursor",
            format!("Invalid cursor: recall cursor decode failed: {err}"),
        )
    })
}
