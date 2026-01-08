use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use serde::de::DeserializeOwned;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

pub const CURSOR_VERSION: u32 = 1;

const MAX_CURSOR_BASE64_CHARS: usize = 8_192;
const MAX_CURSOR_JSON_BYTES: usize = 4_096;

const COMPRESSED_PREFIX_ZLIB_V1: &[u8] = b"CFCZ";

fn compress_zlib(bytes: &[u8]) -> Result<Vec<u8>> {
    // Cursor payloads are tiny but frequent. We trade a bit of CPU for significantly smaller
    // tokens (agent context window is the scarce resource).
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(bytes).context("compress cursor (zlib)")?;
    encoder.finish().context("finish cursor compression")
}

fn decompress_zlib_with_limit(bytes: &[u8], max_len: usize) -> Result<Vec<u8>> {
    let decoder = ZlibDecoder::new(bytes);
    let mut out = Vec::new();
    decoder
        .take(max_len.saturating_add(1) as u64)
        .read_to_end(&mut out)
        .context("decompress cursor (zlib)")?;
    if out.len() > max_len {
        anyhow::bail!("Cursor payload too large ({} bytes)", out.len());
    }
    Ok(out)
}

pub fn encode_cursor<T: Serialize>(cursor: &T) -> Result<String> {
    let bytes = serde_json::to_vec(cursor).context("serialize cursor")?;
    if bytes.len() > MAX_CURSOR_JSON_BYTES {
        anyhow::bail!("Cursor payload too large ({} bytes)", bytes.len());
    }

    let compressed = compress_zlib(&bytes).unwrap_or_default();
    let payload = if !compressed.is_empty()
        && COMPRESSED_PREFIX_ZLIB_V1
            .len()
            .saturating_add(compressed.len())
            < bytes.len()
    {
        let mut out = Vec::with_capacity(COMPRESSED_PREFIX_ZLIB_V1.len() + compressed.len());
        out.extend_from_slice(COMPRESSED_PREFIX_ZLIB_V1);
        out.extend_from_slice(&compressed);
        out
    } else {
        bytes
    };

    Ok(URL_SAFE_NO_PAD.encode(payload))
}

pub fn decode_cursor<T: DeserializeOwned>(cursor: &str) -> Result<T> {
    let cursor = cursor.trim();
    if cursor.is_empty() {
        anyhow::bail!("Cursor must not be empty");
    }
    if cursor.len() > MAX_CURSOR_BASE64_CHARS {
        anyhow::bail!("Cursor too long");
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor.as_bytes())
        .context("decode cursor")?;
    if bytes.len() > MAX_CURSOR_JSON_BYTES {
        anyhow::bail!("Cursor payload too large ({} bytes)", bytes.len());
    }

    let decoded = if bytes.starts_with(COMPRESSED_PREFIX_ZLIB_V1) {
        decompress_zlib_with_limit(
            &bytes[COMPRESSED_PREFIX_ZLIB_V1.len()..],
            MAX_CURSOR_JSON_BYTES,
        )
        .context("decode compressed cursor")?
    } else {
        bytes
    };

    serde_json::from_slice(&decoded).context("parse cursor json")
}

/// Stable 64-bit fingerprint for cursor validation fields.
///
/// Cursor tokens are opaque to clients, but the server uses embedded fields to reject
/// mismatched continuations (wrong root/options). Storing full strings makes cursors large.
/// A short fingerprint keeps validation robust while shrinking tokens.
#[must_use]
pub fn cursor_fingerprint(value: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    u64::from_be_bytes([
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
    ])
}

#[cfg(test)]
mod tests {
    use super::{decode_cursor, encode_cursor, COMPRESSED_PREFIX_ZLIB_V1, MAX_CURSOR_JSON_BYTES};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use serde::{Deserialize, Serialize};
    use std::io::Write;

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct DummyCursor {
        v: u32,
        tool: String,
        payload: String,
    }

    #[test]
    fn roundtrips_small_cursor() {
        let cursor = DummyCursor {
            v: 1,
            tool: "dummy".to_string(),
            payload: "ok".to_string(),
        };
        let token = encode_cursor(&cursor).expect("encode cursor");
        let decoded: DummyCursor = decode_cursor(&token).expect("decode cursor");
        assert_eq!(decoded, cursor);
    }

    #[test]
    fn compressed_encoding_is_backward_compatible() {
        let cursor = DummyCursor {
            v: 1,
            tool: "dummy".to_string(),
            payload: "a".repeat(2048),
        };
        let token = encode_cursor(&cursor).expect("encode cursor");
        let raw = URL_SAFE_NO_PAD
            .decode(token.as_bytes())
            .expect("base64 decode");
        assert!(raw.starts_with(COMPRESSED_PREFIX_ZLIB_V1));

        let decoded: DummyCursor = decode_cursor(&token).expect("decode cursor");
        assert_eq!(decoded, cursor);
    }

    #[test]
    fn decode_rejects_decompressed_payload_over_limit() {
        let payload = "b".repeat(MAX_CURSOR_JSON_BYTES + 128);
        let json = serde_json::to_vec(&DummyCursor {
            v: 1,
            tool: "dummy".to_string(),
            payload,
        })
        .expect("serialize json");
        assert!(json.len() > MAX_CURSOR_JSON_BYTES);

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&json).expect("compress");
        let compressed = encoder.finish().expect("finish compression");

        let mut bytes = Vec::new();
        bytes.extend_from_slice(COMPRESSED_PREFIX_ZLIB_V1);
        bytes.extend_from_slice(&compressed);
        let token = URL_SAFE_NO_PAD.encode(bytes);

        let err = decode_cursor::<serde_json::Value>(&token)
            .expect_err("should reject oversized decompressed cursor");
        let msg = format!("{err:#}");
        assert!(msg.contains("Cursor payload too large"));
    }
}
