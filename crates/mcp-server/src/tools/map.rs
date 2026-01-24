use anyhow::{Context as AnyhowContext, Result};
use context_code_chunker::{Chunker, ChunkerConfig};
use context_indexer::{FileScanner, ToolMeta};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::cursor::{cursor_fingerprint, encode_cursor, CURSOR_VERSION};
use super::paths::normalize_relative_path;
use super::schemas::map::{DirectoryInfo, MapCursorV1, MapResult};
use super::secrets::is_potential_secret_path;
use super::ContextFinderService;

const fn chunker_config_for_map() -> ChunkerConfig {
    ChunkerConfig {
        strategy: context_code_chunker::ChunkingStrategy::Semantic,
        overlap: context_code_chunker::OverlapStrategy::None,
        target_chunk_tokens: 768,
        max_chunk_tokens: 2048,
        min_chunk_tokens: 0,
        include_imports: false,
        include_parent_context: false,
        include_documentation: false,
        max_imports_per_chunk: 0,
        supported_languages: Vec::new(),
    }
}

fn directory_key(file_path: &str, depth: usize) -> String {
    let mut parts: Vec<&str> = file_path.split('/').collect();
    if parts.is_empty() {
        return ".".to_string();
    }
    parts.pop();
    if parts.is_empty() {
        return ".".to_string();
    }
    let depth = depth.min(parts.len());
    parts.into_iter().take(depth).collect::<Vec<_>>().join("/")
}

fn absorb_chunk_for_map(
    tree_files: &mut HashMap<String, HashSet<String>>,
    tree_chunks: &mut HashMap<String, usize>,
    tree_symbols: &mut HashMap<String, Vec<String>>,
    total_lines: &mut usize,
    total_chunks: &mut usize,
    depth: usize,
    chunk: &context_code_chunker::CodeChunk,
) {
    let key = directory_key(&chunk.file_path, depth);

    tree_files
        .entry(key.clone())
        .or_default()
        .insert(chunk.file_path.clone());
    *tree_chunks.entry(key.clone()).or_insert(0) += 1;
    *total_chunks += 1;
    *total_lines += chunk.content.lines().count().max(1);

    if let Some(sym) = &chunk.metadata.symbol_name {
        let sym_type = chunk
            .metadata
            .chunk_type
            .map_or("symbol", context_code_chunker::ChunkType::as_str);
        tree_symbols
            .entry(key)
            .or_default()
            .push(format!("{sym_type} {sym}"));
    }
}

fn compute_coverage_pct(chunks: usize, total_chunks: usize) -> f32 {
    if total_chunks == 0 {
        return 0.0;
    }
    let chunks_u64 = u64::try_from(chunks).unwrap_or(u64::MAX);
    let total_u64 = u64::try_from(total_chunks).unwrap_or(u64::MAX).max(1);
    let bp_u64 = chunks_u64.saturating_mul(10_000) / total_u64;
    let bp = u16::try_from(bp_u64).unwrap_or(u16::MAX);
    f32::from(bp) / 100.0
}

fn compute_top_symbols(tree_symbols: &HashMap<String, Vec<String>>, path: &str) -> Vec<String> {
    tree_symbols
        .get(path)
        .map(|symbols| {
            let mut counts: HashMap<String, usize> = HashMap::new();
            for symbol in symbols {
                *counts.entry(symbol.clone()).or_insert(0) += 1;
            }

            let mut items: Vec<(String, usize)> = counts.into_iter().collect();
            items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            items
                .into_iter()
                .take(5)
                .map(|(symbol, _count)| symbol)
                .collect()
        })
        .unwrap_or_default()
}

fn build_directory_infos(
    tree_files: &HashMap<String, HashSet<String>>,
    tree_symbols: &HashMap<String, Vec<String>>,
    tree_chunks: &HashMap<String, usize>,
    total_chunks: usize,
) -> Vec<DirectoryInfo> {
    let mut keys: Vec<String> = Vec::with_capacity(tree_files.len().max(tree_chunks.len()));
    let mut seen: HashSet<String> = HashSet::new();
    for key in tree_files.keys() {
        if seen.insert(key.clone()) {
            keys.push(key.clone());
        }
    }
    for key in tree_chunks.keys() {
        if seen.insert(key.clone()) {
            keys.push(key.clone());
        }
    }

    keys.into_iter()
        .map(|path| {
            let chunks = tree_chunks.get(&path).copied().unwrap_or(0);
            DirectoryInfo {
                files: Some(
                    tree_files
                        .get(&path)
                        .map_or(0, std::collections::HashSet::len),
                ),
                coverage_pct: Some(compute_coverage_pct(chunks, total_chunks)),
                top_symbols: Some(compute_top_symbols(tree_symbols, &path)),
                path,
                chunks: Some(chunks),
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn populate_map_from_filesystem(
    root: &Path,
    depth: usize,
    scope_prefix: Option<&str>,
    tree_files: &mut HashMap<String, HashSet<String>>,
    tree_chunks: &mut HashMap<String, usize>,
    tree_symbols: &mut HashMap<String, Vec<String>>,
    total_lines: &mut usize,
    total_chunks: &mut usize,
) -> Result<()> {
    let scanner = FileScanner::new(root);
    let files = scanner.scan();
    let chunker = Chunker::new(chunker_config_for_map());

    for file in files {
        let Some(rel_path) = normalize_relative_path(root, &file) else {
            continue;
        };
        if is_potential_secret_path(&rel_path) {
            continue;
        }
        if let Some(prefix) = scope_prefix {
            if !rel_path.starts_with(prefix) {
                continue;
            }
        }

        let key = directory_key(&rel_path, depth);
        tree_files.entry(key).or_default().insert(rel_path.clone());

        let content = match tokio::fs::read_to_string(&file).await {
            Ok(content) => content,
            Err(err) => {
                log::debug!("Skipping unreadable file {}: {err}", file.display());
                continue;
            }
        };
        if content.trim().is_empty() {
            continue;
        }

        let chunks = match chunker.chunk_str(&content, Some(&rel_path)) {
            Ok(chunks) => chunks,
            Err(err) => {
                log::debug!("Skipping unchunkable file {rel_path}: {err}");
                continue;
            }
        };

        for chunk in &chunks {
            absorb_chunk_for_map(
                tree_files,
                tree_chunks,
                tree_symbols,
                total_lines,
                total_chunks,
                depth,
                chunk,
            );
        }
    }

    Ok(())
}

pub(super) async fn compute_map_result(
    root: &Path,
    root_display: &str,
    depth: usize,
    limit: usize,
    offset: usize,
    scope_prefix: Option<&str>,
) -> Result<MapResult> {
    let scope_prefix = scope_prefix
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "." && *s != "./");

    // Aggregate by directory
    let mut tree_files: HashMap<String, HashSet<String>> = HashMap::new();
    let mut tree_chunks: HashMap<String, usize> = HashMap::new();
    let mut tree_symbols: HashMap<String, Vec<String>> = HashMap::new();
    let mut total_lines = 0usize;
    let mut total_chunks = 0usize;

    if let Some(corpus) = ContextFinderService::load_chunk_corpus(root).await? {
        for (file, chunks) in corpus.files() {
            if is_potential_secret_path(file) {
                continue;
            }
            if let Some(prefix) = scope_prefix {
                if !file.starts_with(prefix) {
                    continue;
                }
            }
            let key = directory_key(file, depth);
            tree_files.entry(key).or_default().insert(file.clone());
            for chunk in chunks {
                absorb_chunk_for_map(
                    &mut tree_files,
                    &mut tree_chunks,
                    &mut tree_symbols,
                    &mut total_lines,
                    &mut total_chunks,
                    depth,
                    chunk,
                );
            }
        }
    } else {
        populate_map_from_filesystem(
            root,
            depth,
            scope_prefix,
            &mut tree_files,
            &mut tree_chunks,
            &mut tree_symbols,
            &mut total_lines,
            &mut total_chunks,
        )
        .await?;
    }

    let total_files: usize = tree_files
        .values()
        .map(std::collections::HashSet::len)
        .sum();

    let mut directories =
        build_directory_infos(&tree_files, &tree_symbols, &tree_chunks, total_chunks);

    directories.sort_by(|a, b| b.chunks.cmp(&a.chunks).then_with(|| a.path.cmp(&b.path)));

    if offset > directories.len() {
        anyhow::bail!("Cursor offset out of range (offset={offset})");
    }

    let end = offset.saturating_add(limit).min(directories.len());
    let truncated = end < directories.len();
    let next_cursor = if truncated {
        Some(encode_cursor(&MapCursorV1 {
            v: CURSOR_VERSION,
            tool: "tree".to_string(),
            root: Some(root_display.to_string()),
            root_hash: Some(cursor_fingerprint(root_display)),
            scope: scope_prefix.map(|s| s.to_string()),
            depth,
            limit,
            offset: end,
        })?)
    } else {
        None
    };

    let directories = directories[offset..end].to_vec();

    Ok(MapResult {
        total_files: Some(total_files),
        total_chunks: Some(total_chunks),
        total_lines: Some(total_lines),
        directories,
        truncated,
        next_cursor,
        next_actions: None,
        meta: Some(ToolMeta::default()),
    })
}

pub(super) fn decode_map_cursor(cursor: &str) -> Result<MapCursorV1> {
    super::cursor::decode_cursor(cursor).with_context(|| "decode map cursor")
}

#[cfg(test)]
mod tests {
    use super::directory_key;

    #[test]
    fn directory_key_uses_parent_path() {
        assert_eq!(directory_key("Cargo.toml", 2), ".");
        assert_eq!(directory_key("src/lib.rs", 1), "src");
        assert_eq!(directory_key("src/utils/helpers.rs", 1), "src");
        assert_eq!(directory_key("src/utils/helpers.rs", 2), "src/utils");
    }
}
