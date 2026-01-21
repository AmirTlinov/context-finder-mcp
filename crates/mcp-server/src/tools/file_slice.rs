use anyhow::{Context as AnyhowContext, Result};
use context_indexer::ToolMeta;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Seek};
use std::path::{Path, PathBuf};

use super::cursor::{cursor_fingerprint, decode_cursor, encode_cursor, CURSOR_VERSION};
use super::paths::normalize_relative_path;
use super::schemas::content_format::ContentFormat;
use super::schemas::file_slice::{
    FileSliceCursorV1, FileSliceRequest, FileSliceResult, FileSliceTruncation,
};
use super::schemas::response_mode::ResponseMode;
use super::secrets::is_potential_secret_path;
use super::util::{hex_encode_lower, unix_ms};

const DEFAULT_MAX_LINES: usize = 200;
const MAX_MAX_LINES: usize = 5_000;
const DEFAULT_MAX_CHARS: usize = 2_000;
const MAX_MAX_CHARS: usize = 500_000;

fn file_slice_envelope_reserve(response_mode: ResponseMode, display_file: &str) -> usize {
    // Keep enough headroom for the response envelope so tight `max_chars` budgets still return a
    // useful slice instead of "truncated but empty".
    //
    // We keep this conservative, but noticeably smaller for `.context` output (tiny envelope).
    let file_overhead = display_file
        .chars()
        .count()
        // File path appears in multiple envelope lines (`A:` + `R:`) and can also influence the
        // cursor block; keep a small extra cushion for line numbers and formatting.
        .saturating_mul(2)
        .saturating_add(48)
        .min(768);

    let base_reserve: usize = match response_mode {
        ResponseMode::Minimal => 120,
        ResponseMode::Facts => 200,
        ResponseMode::Full => 380,
    };

    base_reserve.saturating_add(file_overhead)
}

struct CursorValidation<'a> {
    root_display: &'a str,
    root_hash: u64,
    display_file: &'a str,
    max_lines: usize,
    max_chars: usize,
    format: ContentFormat,
    allow_secrets: bool,
    file_size_bytes: u64,
    file_mtime_ms: u64,
}

fn resolve_candidate_path(root: &Path, file_str: &str) -> PathBuf {
    root.join(Path::new(file_str))
}

fn display_file_path(root: &Path, canonical_file: &Path) -> String {
    normalize_relative_path(root, canonical_file).unwrap_or_else(|| {
        canonical_file
            .to_string_lossy()
            .into_owned()
            .replace('\\', "/")
    })
}

fn decode_resume_cursor(
    request: &FileSliceRequest,
    validation: &CursorValidation<'_>,
    start_line: usize,
) -> std::result::Result<(bool, usize, u64), String> {
    let Some(cursor) = request
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok((false, start_line, 0));
    };

    let decoded: FileSliceCursorV1 =
        decode_cursor(cursor).map_err(|err| format!("Invalid cursor: {err}"))?;
    if decoded.v != CURSOR_VERSION || (decoded.tool != "cat" && decoded.tool != "file_slice") {
        return Err("Invalid cursor: wrong tool".to_string());
    }
    if let Some(hash) = decoded.root_hash {
        if hash != validation.root_hash {
            return Err("Invalid cursor: different root".to_string());
        }
    } else if decoded.root.as_deref() != Some(validation.root_display) {
        return Err("Invalid cursor: different root".to_string());
    }
    if decoded.file != validation.display_file {
        return Err("Invalid cursor: different file".to_string());
    }
    if decoded.file_size_bytes != validation.file_size_bytes
        || decoded.file_mtime_ms != validation.file_mtime_ms
    {
        return Err("Invalid cursor: file changed".to_string());
    }
    if decoded.next_byte_offset > validation.file_size_bytes {
        return Err("Invalid cursor: out of range".to_string());
    }

    Ok((
        true,
        decoded.next_start_line.max(1),
        decoded.next_byte_offset,
    ))
}

fn encode_next_cursor(
    validation: &CursorValidation<'_>,
    next_start_line: usize,
    next_byte_offset: u64,
) -> std::result::Result<String, String> {
    let token = FileSliceCursorV1 {
        v: CURSOR_VERSION,
        tool: "cat".to_string(),
        root: Some(validation.root_display.to_string()),
        root_hash: Some(validation.root_hash),
        file: validation.display_file.to_string(),
        max_lines: validation.max_lines,
        max_chars: validation.max_chars,
        format: validation.format,
        allow_secrets: validation.allow_secrets,
        next_start_line,
        next_byte_offset,
        file_size_bytes: validation.file_size_bytes,
        file_mtime_ms: validation.file_mtime_ms,
    };

    encode_cursor(&token).map_err(|err| format!("Error: {err:#}"))
}

struct ReadSliceConfig<'a> {
    canonical_file: &'a Path,
    display_file: &'a str,
    start_line: usize,
    start_byte_offset: u64,
    using_cursor: bool,
    max_lines: usize,
    max_chars: usize,
    format: ContentFormat,
    cursor_validation: &'a CursorValidation<'a>,
}

struct ReadSliceOutcome {
    content: String,
    used_chars: usize,
    returned_lines: usize,
    end_line: usize,
    truncated: bool,
    truncation: Option<FileSliceTruncation>,
    next_cursor: Option<String>,
}

fn read_file_slice(cfg: &ReadSliceConfig<'_>) -> std::result::Result<ReadSliceOutcome, String> {
    let file = std::fs::File::open(cfg.canonical_file)
        .map_err(|e| format!("Failed to open '{}': {e}", cfg.display_file))?;
    let mut reader = BufReader::new(file);
    if cfg.start_byte_offset > 0 {
        reader
            .seek(std::io::SeekFrom::Start(cfg.start_byte_offset))
            .map_err(|e| format!("Failed to seek '{}': {e}", cfg.display_file))?;
    }

    let mut content = String::new();
    let mut used_chars = 0usize;
    let mut returned_lines = 0usize;
    let mut end_line = 0usize;
    let mut truncated = false;
    let mut truncation: Option<FileSliceTruncation> = None;
    let mut next_cursor: Option<String> = None;

    let mut current_offset = cfg.start_byte_offset;
    let mut line_no = if cfg.using_cursor { cfg.start_line } else { 1 };
    let mut buf = String::new();

    loop {
        let pos_before_read = current_offset;
        buf.clear();
        let bytes_read = reader
            .read_line(&mut buf)
            .map_err(|e| format!("Failed to read '{}': {e}", cfg.display_file))?;
        if bytes_read == 0 {
            break;
        }
        current_offset = current_offset.saturating_add(bytes_read as u64);

        let line = buf.trim_end_matches('\n').trim_end_matches('\r');
        if line_no < cfg.start_line {
            line_no = line_no.saturating_add(1);
            continue;
        }

        if returned_lines >= cfg.max_lines {
            truncated = true;
            truncation = Some(FileSliceTruncation::MaxLines);
            next_cursor = Some(encode_next_cursor(
                cfg.cursor_validation,
                line_no,
                pos_before_read,
            )?);
            break;
        }

        let prefix = if cfg.format == ContentFormat::Numbered {
            format!("{line_no}: ")
        } else {
            String::new()
        };

        let line_chars = prefix.chars().count().saturating_add(line.chars().count());
        let extra_chars = if returned_lines == 0 {
            line_chars
        } else {
            1 + line_chars
        };
        if used_chars.saturating_add(extra_chars) > cfg.max_chars {
            truncated = true;
            truncation = Some(FileSliceTruncation::MaxChars);
            next_cursor = Some(encode_next_cursor(
                cfg.cursor_validation,
                line_no,
                pos_before_read,
            )?);
            break;
        }

        if returned_lines > 0 {
            content.push('\n');
            used_chars += 1;
        }
        if !prefix.is_empty() {
            content.push_str(&prefix);
        }
        content.push_str(line);
        used_chars += line_chars;
        returned_lines += 1;
        end_line = line_no;
        line_no = line_no.saturating_add(1);
    }

    Ok(ReadSliceOutcome {
        content,
        used_chars,
        returned_lines,
        end_line,
        truncated,
        truncation,
        next_cursor,
    })
}

pub(super) fn compute_file_slice_result(
    root: &Path,
    root_display: &str,
    request: &FileSliceRequest,
) -> std::result::Result<FileSliceResult, String> {
    let cursor_payload: Option<FileSliceCursorV1> = request
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|cursor| decode_cursor(cursor).map_err(|err| format!("Invalid cursor: {err}")))
        .transpose()?;
    if let Some(decoded) = cursor_payload.as_ref() {
        if decoded.v != CURSOR_VERSION || (decoded.tool != "cat" && decoded.tool != "file_slice") {
            return Err("Invalid cursor: wrong tool".to_string());
        }
        if let Some(hash) = decoded.root_hash {
            if hash != cursor_fingerprint(root_display) {
                return Err("Invalid cursor: different root".to_string());
            }
        } else if decoded.root.as_deref() != Some(root_display) {
            return Err("Invalid cursor: different root".to_string());
        }
    }

    let requested_file = request
        .file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let file_str = if let Some(file) = requested_file {
        file
    } else if let Some(decoded) = cursor_payload.as_ref() {
        decoded.file.as_str()
    } else {
        return Err("Error: file is required when no cursor is provided".to_string());
    };

    let candidate = resolve_candidate_path(root, file_str);

    let canonical_file = match candidate.canonicalize() {
        Ok(p) => p,
        Err(e) => return Err(format!("Invalid file '{file_str}': {e}")),
    };

    if !canonical_file.starts_with(root) {
        return Err(format!("File '{file_str}' is outside project root"));
    }

    let display_file = display_file_path(root, &canonical_file);

    let allow_secrets = request
        .allow_secrets
        .or_else(|| cursor_payload.as_ref().map(|c| c.allow_secrets))
        .unwrap_or(false);
    if !allow_secrets && is_potential_secret_path(&display_file) {
        return Err(format!(
            "Refusing to read potential secret file '{display_file}' (set allow_secrets=true to override)"
        ));
    }

    let meta = match std::fs::metadata(&canonical_file) {
        Ok(m) => m,
        Err(e) => return Err(format!("Failed to stat '{display_file}': {e}")),
    };
    let file_size_bytes = meta.len();
    let file_mtime_ms = meta.modified().map(unix_ms).unwrap_or(0);

    let max_lines = request
        .max_lines
        .or_else(|| cursor_payload.as_ref().map(|c| c.max_lines))
        .unwrap_or(DEFAULT_MAX_LINES)
        .clamp(1, MAX_MAX_LINES);
    let output_max_chars = request
        .max_chars
        .or_else(|| cursor_payload.as_ref().map(|c| c.max_chars))
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(1, MAX_MAX_CHARS);

    let start_line = request.start_line.unwrap_or(1).max(1);
    let format = request
        .format
        .or_else(|| cursor_payload.as_ref().map(|c| c.format))
        .unwrap_or(ContentFormat::Plain);
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Minimal);

    // `max_chars` is a hard budget for the whole tool output. Reserve envelope headroom and spend
    // the rest on actual file content.
    //
    // Guardrail: keep `reserve` from starving content in small budgets (common in tests and
    // "just show me a snippet" workflows). We'll still respect `max_chars` overall, but aim to
    // leave enough room for at least a couple of meaningful lines.
    let reserve = {
        let min_content = match response_mode {
            ResponseMode::Minimal => 120,
            ResponseMode::Facts => 200,
            ResponseMode::Full => 260,
        };
        let raw = file_slice_envelope_reserve(response_mode, &display_file);
        raw.min(output_max_chars.saturating_sub(min_content))
    };
    let content_max_chars = output_max_chars.saturating_sub(reserve).max(1);
    let validation = CursorValidation {
        root_display,
        root_hash: cursor_fingerprint(root_display),
        display_file: &display_file,
        max_lines,
        max_chars: output_max_chars,
        format,
        allow_secrets,
        file_size_bytes,
        file_mtime_ms,
    };
    let request_with_cursor_filled = FileSliceRequest {
        path: request.path.clone(),
        file: Some(display_file.clone()),
        start_line: request.start_line,
        max_lines: request.max_lines,
        max_chars: request.max_chars,
        format: request.format,
        response_mode: request.response_mode,
        allow_secrets: request.allow_secrets,
        cursor: request.cursor.clone(),
    };
    let (using_cursor, start_line, start_byte_offset) =
        decode_resume_cursor(&request_with_cursor_filled, &validation, start_line)?;

    let read_cfg = ReadSliceConfig {
        canonical_file: &canonical_file,
        display_file: &display_file,
        start_line,
        start_byte_offset,
        using_cursor,
        max_lines,
        max_chars: content_max_chars,
        format,
        cursor_validation: &validation,
    };
    let read = read_file_slice(&read_cfg)?;

    let mut hasher = Sha256::new();
    hasher.update(read.content.as_bytes());
    let content_sha256 = hex_encode_lower(&hasher.finalize());

    Ok(FileSliceResult {
        file: display_file,
        start_line,
        end_line: read.end_line,
        returned_lines: Some(read.returned_lines),
        used_chars: Some(read.used_chars),
        max_lines: Some(max_lines),
        max_chars: Some(output_max_chars),
        truncated: read.truncated,
        truncation: read.truncation,
        next_cursor: read.next_cursor,
        next_actions: None,
        meta: Some(ToolMeta::default()),
        file_size_bytes: Some(file_size_bytes),
        file_mtime_ms: Some(file_mtime_ms),
        content_sha256: Some(content_sha256),
        content: read.content,
    })
}

pub(super) fn compute_onboarding_doc_slice(
    root: &Path,
    file: &str,
    start_line: usize,
    max_lines: usize,
    max_chars: usize,
) -> Result<FileSliceResult> {
    let file = file.trim();
    if file.is_empty() {
        anyhow::bail!("Doc file path must not be empty");
    }

    let canonical_file = resolve_candidate_path(root, file)
        .canonicalize()
        .with_context(|| format!("Failed to resolve doc path '{file}'"))?;
    if !canonical_file.starts_with(root) {
        anyhow::bail!("Doc file '{file}' is outside project root");
    }

    let display_file = normalize_relative_path(root, &canonical_file).unwrap_or_else(|| {
        canonical_file
            .to_string_lossy()
            .into_owned()
            .replace('\\', "/")
    });

    let meta = std::fs::metadata(&canonical_file)
        .with_context(|| format!("Failed to stat '{display_file}'"))?;
    let file_size_bytes = meta.len();
    let file_mtime_ms = meta.modified().map(unix_ms).unwrap_or(0);

    let file = std::fs::File::open(&canonical_file)
        .with_context(|| format!("Failed to open '{display_file}'"))?;
    let reader = BufReader::new(file);

    let mut content = String::new();
    let mut used_chars = 0usize;
    let mut returned_lines = 0usize;
    let mut end_line = 0usize;
    let mut truncated = false;
    let mut truncation: Option<FileSliceTruncation> = None;

    for (idx, line) in reader.lines().enumerate() {
        let line_no = idx + 1;
        let line = line.with_context(|| format!("Failed to read '{display_file}'"))?;

        if line_no < start_line {
            continue;
        }

        if returned_lines >= max_lines {
            truncated = true;
            truncation = Some(FileSliceTruncation::MaxLines);
            break;
        }

        let line_chars = line.chars().count();
        let extra_chars = if returned_lines == 0 {
            line_chars
        } else {
            1 + line_chars
        };
        if used_chars.saturating_add(extra_chars) > max_chars {
            truncated = true;
            truncation = Some(FileSliceTruncation::MaxChars);
            break;
        }

        if returned_lines > 0 {
            content.push('\n');
            used_chars += 1;
        }
        content.push_str(&line);
        used_chars += line_chars;
        returned_lines += 1;
        end_line = line_no;
    }

    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let content_sha256 = hex_encode_lower(&hasher.finalize());

    Ok(FileSliceResult {
        file: display_file,
        start_line,
        end_line,
        returned_lines: Some(returned_lines),
        used_chars: Some(used_chars),
        max_lines: Some(max_lines),
        max_chars: Some(max_chars),
        truncated,
        truncation,
        next_cursor: None,
        next_actions: None,
        meta: Some(ToolMeta::default()),
        file_size_bytes: Some(file_size_bytes),
        file_mtime_ms: Some(file_mtime_ms),
        content_sha256: Some(content_sha256),
        content,
    })
}
