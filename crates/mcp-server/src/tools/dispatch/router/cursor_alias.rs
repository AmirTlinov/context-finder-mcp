use super::super::{decode_cursor, ContextFinderService, CURSOR_VERSION};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::Value;
use sha2::{Digest, Sha256};

// Keep cursors cheap for the agent context window.
//
// Inline cursors are stateless (nice for portability), but even "medium" cursor strings add up
// quickly in tight loops. We bias toward compact server-stored cursor aliases for anything more
// than a tiny token.
// Keep this very small: agents often paste cursors through multiple calls, and even 30–40 chars
// become noticeable noise across tight loops. Stored aliases are persisted best-effort and TTLed.
const MAX_INLINE_CURSOR_CHARS: usize = 16;

/// Compact cursor tokens are a UX feature: they keep continuations cheap for the agent context
/// window by storing large inline cursors server-side (short-lived) and returning a small alias.
///
/// Prefix contains ':' which is not part of base64 URL_SAFE_NO_PAD, so it won't collide with
/// inline cursor encodings.
const CURSOR_ALIAS_PREFIX_V1: &str = "cfcs1:";
const CURSOR_ALIAS_PREFIX_V2: &str = "cfcs2:";
const CURSOR_ALIAS_SIG_BYTES: usize = 6;

fn stored_cursor_id(value: &Value) -> Option<u64> {
    if value.get("mode").and_then(Value::as_str) != Some("stored") {
        return None;
    }
    if value.get("v").and_then(Value::as_u64) != Some(u64::from(CURSOR_VERSION)) {
        return None;
    }
    value.get("store_id").and_then(Value::as_u64)
}

fn decode_store_id(encoded: &str) -> Option<u64> {
    let bytes = URL_SAFE_NO_PAD.decode(encoded.as_bytes()).ok()?;
    let bytes: [u8; 8] = bytes.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

fn encode_store_id_v2(id: u64, sig: [u8; CURSOR_ALIAS_SIG_BYTES]) -> String {
    let mut bytes = [0u8; 8 + CURSOR_ALIAS_SIG_BYTES];
    bytes[..8].copy_from_slice(&id.to_be_bytes());
    bytes[8..].copy_from_slice(&sig);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn decode_store_id_v2(encoded: &str) -> Option<(u64, [u8; CURSOR_ALIAS_SIG_BYTES])> {
    let bytes = URL_SAFE_NO_PAD.decode(encoded.as_bytes()).ok()?;
    let bytes: [u8; 8 + CURSOR_ALIAS_SIG_BYTES] = bytes.try_into().ok()?;
    let id_bytes: [u8; 8] = bytes[..8].try_into().ok()?;
    let sig: [u8; CURSOR_ALIAS_SIG_BYTES] = bytes[8..].try_into().ok()?;
    Some((u64::from_be_bytes(id_bytes), sig))
}

fn payload_sig(payload: &[u8]) -> [u8; CURSOR_ALIAS_SIG_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(payload);
    let digest = hasher.finalize();
    let mut sig = [0u8; CURSOR_ALIAS_SIG_BYTES];
    sig.copy_from_slice(&digest[..CURSOR_ALIAS_SIG_BYTES]);
    sig
}

fn strip_alias_prefix_v1(cursor: &str) -> Option<u64> {
    let cursor = cursor.trim();
    let encoded = cursor.strip_prefix(CURSOR_ALIAS_PREFIX_V1)?;
    decode_store_id(encoded)
}

fn strip_alias_prefix_v2(cursor: &str) -> Option<(u64, [u8; CURSOR_ALIAS_SIG_BYTES])> {
    let cursor = cursor.trim();
    let encoded = cursor.strip_prefix(CURSOR_ALIAS_PREFIX_V2)?;
    decode_store_id_v2(encoded)
}

pub(super) async fn expand_cursor_alias(
    service: &ContextFinderService,
    cursor: &str,
) -> Result<String, String> {
    let cursor = cursor.trim();
    if cursor.is_empty() {
        return Ok(cursor.to_string());
    }

    // Compact cursor aliases (server-stored, TTLed). `cfcs2` includes a short payload signature so
    // even an ID collision or store corruption fails closed (expired), never to a wrong cursor.
    if let Some((store_id, sig)) = strip_alias_prefix_v2(cursor) {
        let Some(bytes) = service.state.cursor_store_get(store_id).await else {
            return Err("Invalid cursor: expired continuation".to_string());
        };
        if payload_sig(&bytes) != sig {
            return Err("Invalid cursor: expired continuation".to_string());
        }
        return String::from_utf8(bytes)
            .map_err(|_| "Invalid cursor: stored continuation invalid".to_string());
    }

    // Legacy compact cursor alias: `cfcs1:<base64(u64)>` (no signature).
    if let Some(store_id) = strip_alias_prefix_v1(cursor) {
        let Some(bytes) = service.state.cursor_store_get(store_id).await else {
            return Err("Invalid cursor: expired continuation".to_string());
        };
        return String::from_utf8(bytes)
            .map_err(|_| "Invalid cursor: stored continuation invalid".to_string());
    }

    let value: Value = match decode_cursor(cursor) {
        Ok(v) => v,
        Err(_) => return Ok(cursor.to_string()),
    };

    let Some(store_id) = stored_cursor_id(&value) else {
        return Ok(cursor.to_string());
    };
    let Some(bytes) = service.state.cursor_store_get(store_id).await else {
        return Err("Invalid cursor: expired continuation".to_string());
    };
    String::from_utf8(bytes).map_err(|_| "Invalid cursor: stored continuation invalid".to_string())
}

pub(super) async fn compact_cursor_alias(service: &ContextFinderService, cursor: String) -> String {
    let trimmed = cursor.trim();
    if trimmed.is_empty() {
        return cursor;
    }
    if trimmed.len() <= MAX_INLINE_CURSOR_CHARS {
        return cursor;
    }
    if trimmed.starts_with(CURSOR_ALIAS_PREFIX_V1) {
        return cursor;
    }
    if trimmed.starts_with(CURSOR_ALIAS_PREFIX_V2) {
        return cursor;
    }

    let value = match decode_cursor::<Value>(trimmed) {
        Ok(value) => value,
        Err(_) => return cursor,
    };
    if stored_cursor_id(&value).is_some() {
        return cursor;
    }

    let payload = trimmed.as_bytes().to_vec();
    let sig = payload_sig(&payload);
    let store_id = service.state.cursor_store_put(payload).await;
    format!(
        "{CURSOR_ALIAS_PREFIX_V2}{}",
        encode_store_id_v2(store_id, sig)
    )
}
