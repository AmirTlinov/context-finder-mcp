use anyhow::{anyhow, Context as AnyhowContext, Result};
use context_indexer::ToolMeta;
use context_protocol::enforce_max_chars;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader};
use std::path::Path;

use super::paths::normalize_relative_path;
use super::schemas::evidence_fetch::{
    EvidenceFetchBudget, EvidenceFetchItem, EvidenceFetchRequest, EvidenceFetchResult,
    EvidenceFetchTruncation, EvidencePointer,
};
use super::schemas::response_mode::ResponseMode;
use super::secrets::is_potential_secret_path;

const VERSION: u32 = 1;
const DEFAULT_MAX_CHARS: usize = 2_000;
const MIN_MAX_CHARS: usize = 800;
const MAX_MAX_CHARS: usize = 500_000;
const DEFAULT_MAX_LINES: usize = 200;
const MAX_MAX_LINES: usize = 5_000;

pub(super) async fn compute_evidence_fetch_result(
    root: &Path,
    request: &EvidenceFetchRequest,
) -> Result<EvidenceFetchResult> {
    let _response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let max_chars = request
        .max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);
    let max_lines = request
        .max_lines
        .unwrap_or(DEFAULT_MAX_LINES)
        .clamp(1, MAX_MAX_LINES);
    let strict_hash = request.strict_hash.unwrap_or(false);

    let mut items: Vec<EvidenceFetchItem> = Vec::new();
    for pointer in &request.items {
        let file = pointer.file.trim();
        if file.is_empty() {
            return Err(anyhow!("Evidence file path must not be empty"));
        }
        let rel = file.replace('\\', "/");
        if is_potential_secret_path(&rel) {
            return Err(anyhow!(
                "Refusing to read potential secret path: {rel} (use file_slice with allow_secrets=true if you really mean it)"
            ));
        }

        let canonical = root
            .join(Path::new(&rel))
            .canonicalize()
            .with_context(|| format!("Failed to resolve evidence path '{rel}'"))?;
        if !canonical.starts_with(root) {
            return Err(anyhow!("Evidence file '{rel}' is outside project root"));
        }
        let display_file = normalize_relative_path(root, &canonical).unwrap_or(rel);

        let (hash, file_lines) = hash_and_count_lines(&canonical)?;
        let stale = pointer
            .source_hash
            .as_deref()
            .map(|expected| expected != hash)
            .unwrap_or(false);
        if stale && strict_hash {
            return Err(anyhow!(
                "Evidence source_hash mismatch for {display_file} (expected={}, got={hash})",
                pointer.source_hash.as_deref().unwrap_or("<missing>")
            ));
        }

        let start_line = pointer.start_line.max(1);
        let end_line = pointer.end_line.max(start_line).min(file_lines.max(1));

        let (content, truncated) = read_lines_window(&canonical, start_line, end_line, max_lines)?;

        items.push(EvidenceFetchItem {
            evidence: EvidencePointer {
                file: display_file,
                start_line,
                end_line,
                source_hash: Some(hash),
            },
            content,
            truncated,
            stale,
        });
    }

    let mut result = EvidenceFetchResult {
        version: VERSION,
        items,
        budget: EvidenceFetchBudget {
            max_chars,
            used_chars: 0,
            truncated: false,
            truncation: None,
        },
        next_actions: Vec::new(),
        meta: ToolMeta::default(),
    };

    trim_to_budget(&mut result)?;
    Ok(result)
}

fn trim_to_budget(result: &mut EvidenceFetchResult) -> anyhow::Result<()> {
    let max_chars = result.budget.max_chars;
    let used = enforce_max_chars(
        result,
        max_chars,
        |inner, used| inner.budget.used_chars = used,
        |inner| {
            inner.budget.truncated = true;
            inner.budget.truncation = Some(EvidenceFetchTruncation::MaxChars);
        },
        |inner| {
            if inner.items.len() > 1 {
                inner.items.pop();
                return true;
            }
            if let Some(item) = inner.items.first_mut() {
                if item.content.is_empty() {
                    return false;
                }
                item.truncated = true;
                let target = item.content.chars().count().saturating_sub(200);
                item.content = item.content.chars().take(target).collect::<String>();
                return true;
            }
            false
        },
    )?;
    result.budget.used_chars = used;
    Ok(())
}

fn hash_and_count_lines(path: &Path) -> Result<(String, usize)> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("Failed to stat {}", path.display()))?;
    let file_size = meta.len();

    let file =
        std::fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut reader = BufReader::new(file);

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut newlines = 0usize;
    loop {
        let n = std::io::Read::read(&mut reader, &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        newlines += buf[..n].iter().filter(|&&b| b == b'\n').count();
    }
    let hash = format!("{:x}", hasher.finalize());
    let lines = if file_size == 0 { 0 } else { newlines + 1 };
    Ok((hash, lines))
}

fn read_lines_window(
    path: &Path,
    start_line: usize,
    end_line: usize,
    max_lines: usize,
) -> Result<(String, bool)> {
    let file =
        std::fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut current = 0usize;
    let mut out: Vec<String> = Vec::new();
    let mut truncated = false;
    for line in reader.lines() {
        current += 1;
        if current < start_line {
            continue;
        }
        if current > end_line {
            break;
        }
        out.push(line?);
        if out.len() >= max_lines {
            truncated = true;
            break;
        }
    }
    Ok((out.join("\n"), truncated))
}
