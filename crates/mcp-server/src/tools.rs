//! MCP Tools for Context Finder
//!
//! Provides semantic code search capabilities to AI agents via MCP protocol.

use crate::runtime_env;
use anyhow::{Context as AnyhowContext, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use context_code_chunker::{Chunker, ChunkerConfig};
use context_graph::{
    build_graph_docs, CodeGraph, ContextAssembler, GraphDocConfig, GraphEdge, GraphLanguage,
    GraphNode, RelationshipType, Symbol, GRAPH_DOC_VERSION,
};
use context_indexer::FileScanner;
use context_search::{
    ContextPackBudget, ContextPackItem, ContextPackOutput, MultiModelContextSearch,
    MultiModelHybridSearch, QueryClassifier, QueryType, SearchProfile, CONTEXT_PACK_VERSION,
};
use context_vector_store::{
    corpus_path_for_project_root, current_model_id, ChunkCorpus, GraphNodeDoc, GraphNodeStore,
    GraphNodeStoreMeta, QueryKind, VectorIndex,
};
use regex::{Regex, RegexBuilder};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// Context Finder MCP Service
#[derive(Clone)]
pub struct ContextFinderService {
    /// Search profile
    profile: SearchProfile,
    /// Tool router
    tool_router: ToolRouter<Self>,
    /// Shared cache state (per-process)
    state: Arc<ServiceState>,
}

impl ContextFinderService {
    pub fn new() -> Self {
        Self {
            profile: load_profile_from_env(),
            tool_router: Self::tool_router(),
            state: Arc::new(ServiceState::new()),
        }
    }
}

fn load_profile_from_env() -> SearchProfile {
    let profile_name = std::env::var("CONTEXT_FINDER_PROFILE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "quality".to_string());

    if let Some(profile) = SearchProfile::builtin(&profile_name) {
        return profile;
    }

    let candidate_path = PathBuf::from(&profile_name);
    if candidate_path.exists() {
        match SearchProfile::from_file(&profile_name, &candidate_path) {
            Ok(profile) => return profile,
            Err(err) => {
                log::warn!(
                    "Failed to load profile from {}: {err:#}; falling back to builtin 'quality'",
                    candidate_path.display()
                );
            }
        }
    } else {
        log::warn!("Unknown profile '{profile_name}', falling back to builtin 'quality'");
    }

    SearchProfile::builtin("quality").unwrap_or_else(SearchProfile::general)
}

#[tool_handler]
impl ServerHandler for ContextFinderService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some("Context Finder provides semantic code search for AI agents. Use 'map' to explore project structure, 'search' for semantic queries, 'context' for search with related code, 'index' to index new projects, and 'doctor' to diagnose model/GPU/index configuration.".into()),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            ..Default::default()
        }
    }
}

impl ContextFinderService {
    async fn load_chunk_corpus(root: &Path) -> Result<Option<ChunkCorpus>> {
        let path = corpus_path_for_project_root(root);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(ChunkCorpus::load(&path).await.with_context(|| {
            format!("Failed to load chunk corpus {}", path.display())
        })?))
    }

    async fn lock_engine(&self, root: &Path) -> Result<EngineLock> {
        self.touch_daemon_best_effort(root);

        let handle = self.state.engine_handle(root).await;
        let mut slot = handle.lock_owned().await;

        let signature = compute_engine_signature(root, &self.profile).await?;
        let needs_rebuild = match slot.engine.as_ref() {
            Some(engine) => engine.signature != signature,
            None => true,
        };
        if needs_rebuild {
            slot.engine = None;
            slot.engine = Some(build_project_engine(root, &self.profile, signature).await?);
        }

        Ok(EngineLock { slot })
    }

    fn touch_daemon_best_effort(&self, root: &Path) {
        let disable = std::env::var("CONTEXT_FINDER_DISABLE_DAEMON")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if disable {
            return;
        }

        let root = root.to_path_buf();
        tokio::spawn(async move {
            if let Err(err) = crate::daemon::touch(&root).await {
                log::debug!("daemon touch failed: {err:#}");
            }
        });
    }

    async fn auto_index_project(&self, root: &Path) -> Result<()> {
        let templates = self.profile.embedding().clone();
        let indexer =
            context_indexer::ProjectIndexer::new_with_embedding_templates(root, templates).await?;
        match indexer.index().await {
            Ok(_) => Ok(()),
            Err(err) => {
                log::warn!("Auto-index failed, retrying full: {err:#}");
                indexer.index_full().await?;
                Ok(())
            }
        }
    }
}

fn model_id_dir_name(model_id: &str) -> String {
    model_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

fn graph_nodes_store_path(root: &Path) -> PathBuf {
    let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    root.join(".context-finder")
        .join("indexes")
        .join(model_id_dir_name(&model_id))
        .join("graph_nodes.json")
}

fn index_path_for_model(root: &Path, model_id: &str) -> PathBuf {
    root.join(".context-finder")
        .join("indexes")
        .join(model_id_dir_name(model_id))
        .join("index.json")
}

fn semantic_model_roster(profile: &SearchProfile) -> Vec<String> {
    let experts = profile.experts();
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for kind in [
        QueryKind::Identifier,
        QueryKind::Path,
        QueryKind::Conceptual,
    ] {
        for model_id in experts.semantic_models(kind) {
            if seen.insert(model_id.clone()) {
                out.push(model_id.clone());
            }
        }
    }

    out
}

async fn load_semantic_indexes(
    root: &Path,
    profile: &SearchProfile,
) -> Result<Vec<(String, VectorIndex)>> {
    let default_model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());

    let mut requested: Vec<String> = Vec::new();
    requested.push(default_model_id.clone());
    requested.extend(semantic_model_roster(profile));

    let mut sources = Vec::new();
    let mut seen = HashSet::new();
    for model_id in requested {
        if !seen.insert(model_id.clone()) {
            continue;
        }
        let path = index_path_for_model(root, &model_id);
        if !path.exists() {
            continue;
        }
        let index = VectorIndex::load(&path)
            .await
            .with_context(|| format!("Failed to load index {}", path.display()))?;
        sources.push((model_id, index));
    }

    if sources.is_empty() {
        anyhow::bail!("No semantic indices available (run 'context-finder index' first)");
    }

    Ok(sources)
}

fn build_chunk_lookup(chunks: &[context_code_chunker::CodeChunk]) -> HashMap<String, usize> {
    let mut lookup = HashMap::new();
    for (idx, chunk) in chunks.iter().enumerate() {
        lookup.insert(
            format!(
                "{}:{}:{}",
                chunk.file_path, chunk.start_line, chunk.end_line
            ),
            idx,
        );
    }
    lookup
}

fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn normalize_relative_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let rel = rel.to_string_lossy().into_owned();
    Some(rel.replace('\\', "/"))
}

const CURSOR_VERSION: u32 = 1;
const MAX_CURSOR_BASE64_CHARS: usize = 8_192;
const MAX_CURSOR_JSON_BYTES: usize = 4_096;

fn encode_cursor<T: Serialize>(cursor: &T) -> Result<String> {
    let bytes = serde_json::to_vec(cursor).context("serialize cursor")?;
    if bytes.len() > MAX_CURSOR_JSON_BYTES {
        anyhow::bail!("Cursor payload too large ({} bytes)", bytes.len());
    }
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn decode_cursor<T: DeserializeOwned>(cursor: &str) -> Result<T> {
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
    serde_json::from_slice(&bytes).context("parse cursor json")
}

fn chunker_config_for_map() -> ChunkerConfig {
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
        supported_languages: vec![],
    }
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
    let parts: Vec<&str> = chunk.file_path.split('/').collect();
    let key = parts
        .iter()
        .take(depth)
        .cloned()
        .collect::<Vec<_>>()
        .join("/");

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
            .map(|ct| ct.as_str())
            .unwrap_or("symbol");
        tree_symbols
            .entry(key)
            .or_default()
            .push(format!("{} {}", sym_type, sym));
    }
}

async fn compute_map_result(
    root: &Path,
    root_display: &str,
    depth: usize,
    limit: usize,
    offset: usize,
) -> Result<MapResult> {
    // Aggregate by directory
    let mut tree_files: HashMap<String, HashSet<String>> = HashMap::new();
    let mut tree_chunks: HashMap<String, usize> = HashMap::new();
    let mut tree_symbols: HashMap<String, Vec<String>> = HashMap::new();
    let mut total_lines = 0usize;
    let mut total_chunks = 0usize;

    match ContextFinderService::load_chunk_corpus(root).await? {
        Some(corpus) => {
            for chunks in corpus.files().values() {
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
        }
        None => {
            let scanner = FileScanner::new(root);
            let files = scanner.scan();
            let chunker = Chunker::new(chunker_config_for_map());

            for file in files {
                let Some(rel_path) = normalize_relative_path(root, &file) else {
                    continue;
                };

                let parts: Vec<&str> = rel_path.split('/').collect();
                let key = parts
                    .iter()
                    .take(depth)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("/");
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
        }
    }

    let total_files: usize = tree_files.values().map(|s| s.len()).sum();

    let mut directories: Vec<DirectoryInfo> = tree_chunks
        .into_iter()
        .map(|(path, chunks)| {
            let files = tree_files.get(&path).map(|s| s.len()).unwrap_or(0);
            let coverage_pct = if total_chunks > 0 {
                chunks as f32 / total_chunks as f32 * 100.0
            } else {
                0.0
            };
            let top_symbols = tree_symbols
                .get(&path)
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
                .unwrap_or_default();

            DirectoryInfo {
                path,
                files,
                chunks,
                coverage_pct,
                top_symbols,
            }
        })
        .collect();

    directories.sort_by(|a, b| b.chunks.cmp(&a.chunks).then_with(|| a.path.cmp(&b.path)));

    if offset > directories.len() {
        anyhow::bail!("Cursor offset out of range (offset={offset})");
    }

    let end = offset.saturating_add(limit).min(directories.len());
    let truncated = end < directories.len();
    let next_cursor = if truncated {
        Some(encode_cursor(&MapCursorV1 {
            v: CURSOR_VERSION,
            tool: "map".to_string(),
            root: root_display.to_string(),
            depth,
            offset: end,
        })?)
    } else {
        None
    };

    let directories = directories[offset..end].to_vec();

    Ok(MapResult {
        total_files,
        total_chunks,
        total_lines,
        directories,
        truncated,
        next_cursor,
    })
}

async fn compute_list_files_result(
    root: &Path,
    root_display: &str,
    file_pattern: Option<&str>,
    limit: usize,
    max_chars: usize,
    cursor_last_file: Option<&str>,
) -> Result<ListFilesResult> {
    let file_pattern = file_pattern.map(str::trim).filter(|s| !s.is_empty());
    let cursor_last_file = cursor_last_file.map(str::trim).filter(|s| !s.is_empty());

    let mut used_chars = 0usize;
    let mut truncated = false;
    let mut truncation: Option<ListFilesTruncation> = None;
    let mut files: Vec<String> = Vec::new();
    let mut next_cursor: Option<String> = None;
    let source: String;
    let scanned_files: usize;
    let mut matched: Vec<String> = Vec::new();

    match ContextFinderService::load_chunk_corpus(root).await? {
        Some(corpus) => {
            source = "corpus".to_string();

            let mut candidates: Vec<&String> = corpus.files().keys().collect();
            candidates.sort();
            scanned_files = candidates.len();

            for file in candidates {
                if !ContextFinderService::matches_file_pattern(file, file_pattern) {
                    continue;
                }
                matched.push(file.clone());
            }
        }
        None => {
            source = "filesystem".to_string();

            let scanner = FileScanner::new(root);
            let scanned = scanner.scan();
            scanned_files = scanned.len();

            let mut candidates: Vec<String> = scanned
                .into_iter()
                .filter_map(|p| normalize_relative_path(root, &p))
                .collect();
            candidates.sort();

            for file in candidates {
                if !ContextFinderService::matches_file_pattern(&file, file_pattern) {
                    continue;
                }
                matched.push(file);
            }
        }
    }

    let start_index = if let Some(last) = cursor_last_file {
        match matched.binary_search_by(|candidate| candidate.as_str().cmp(last)) {
            Ok(idx) => idx + 1,
            Err(idx) => idx,
        }
    } else {
        0
    };

    if start_index > matched.len() {
        anyhow::bail!("Cursor is out of range for matched files");
    }

    for file in matched.iter().skip(start_index) {
        if files.len() >= limit {
            truncated = true;
            truncation = Some(ListFilesTruncation::Limit);
            break;
        }

        let file_chars = file.chars().count();
        let extra_chars = if files.is_empty() {
            file_chars
        } else {
            1 + file_chars
        };
        if used_chars.saturating_add(extra_chars) > max_chars {
            truncated = true;
            truncation = Some(ListFilesTruncation::MaxChars);
            break;
        }

        files.push(file.clone());
        used_chars += extra_chars;
    }

    if truncated && !files.is_empty() && start_index.saturating_add(files.len()) < matched.len() {
        if let Some(last_file) = files.last() {
            next_cursor = Some(encode_cursor(&ListFilesCursorV1 {
                v: CURSOR_VERSION,
                tool: "list_files".to_string(),
                root: root_display.to_string(),
                file_pattern: file_pattern.map(str::to_string),
                last_file: last_file.clone(),
            })?);
        }
    }

    Ok(ListFilesResult {
        source,
        file_pattern: file_pattern.map(str::to_string),
        scanned_files,
        returned: files.len(),
        used_chars,
        limit,
        max_chars,
        truncated,
        truncation,
        next_cursor,
        files,
    })
}

#[derive(Debug, Clone)]
struct GrepRange {
    start_line: usize,
    end_line: usize,
    match_lines: Vec<usize>,
}

fn merge_grep_ranges(mut ranges: Vec<GrepRange>) -> Vec<GrepRange> {
    ranges.sort_by(|a, b| {
        a.start_line
            .cmp(&b.start_line)
            .then_with(|| a.end_line.cmp(&b.end_line))
    });

    let mut merged: Vec<GrepRange> = Vec::new();
    for range in ranges {
        let Some(last) = merged.last_mut() else {
            merged.push(range);
            continue;
        };

        if range.start_line <= last.end_line.saturating_add(1) {
            last.end_line = last.end_line.max(range.end_line);
            last.match_lines.extend(range.match_lines);
            continue;
        }

        merged.push(range);
    }

    for range in &mut merged {
        range.match_lines.sort_unstable();
        range.match_lines.dedup();
    }

    merged
}

struct GrepContextComputeOptions<'a> {
    case_sensitive: bool,
    before: usize,
    after: usize,
    max_matches: usize,
    max_hunks: usize,
    max_chars: usize,
    resume_file: Option<&'a str>,
    resume_line: usize,
}

async fn compute_grep_context_result(
    root: &Path,
    root_display: &str,
    request: &GrepContextRequest,
    regex: &Regex,
    opts: GrepContextComputeOptions<'_>,
) -> Result<GrepContextResult> {
    const MAX_FILE_BYTES: u64 = 2_000_000;

    let GrepContextComputeOptions {
        case_sensitive,
        before,
        after,
        max_matches,
        max_hunks,
        max_chars,
        resume_file,
        resume_line,
    } = opts;

    let file_pattern = request
        .file_pattern
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let resume_file = resume_file.map(str::trim).filter(|s| !s.is_empty());
    let resume_line = resume_line.max(1);

    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    let mut source = "filesystem".to_string();

    if let Some(file) = request
        .file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let input_path = Path::new(file);
        let candidate = if input_path.is_absolute() {
            PathBuf::from(input_path)
        } else {
            root.join(input_path)
        };
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("Invalid file '{file}'"))?;
        if !canonical.starts_with(root) {
            anyhow::bail!("File '{file}' is outside project root");
        }
        let display = normalize_relative_path(root, &canonical)
            .unwrap_or_else(|| canonical.to_string_lossy().into_owned().replace('\\', "/"));
        candidates.push((display, canonical));
    } else {
        match ContextFinderService::load_chunk_corpus(root).await? {
            Some(corpus) => {
                source = "corpus".to_string();
                let mut files: Vec<&String> = corpus.files().keys().collect();
                files.sort();
                for file in files {
                    if !ContextFinderService::matches_file_pattern(file, file_pattern) {
                        continue;
                    }
                    candidates.push((file.clone(), root.join(file)));
                }
            }
            None => {
                let scanner = FileScanner::new(root);
                let files = scanner.scan();
                let mut rels: Vec<String> = files
                    .into_iter()
                    .filter_map(|p| normalize_relative_path(root, &p))
                    .collect();
                rels.sort();
                for rel in rels {
                    if !ContextFinderService::matches_file_pattern(&rel, file_pattern) {
                        continue;
                    }
                    candidates.push((rel.clone(), root.join(&rel)));
                }
            }
        }
    }

    if let Some(resume_file) = resume_file {
        if !candidates.iter().any(|(file, _)| file == resume_file) {
            anyhow::bail!("Cursor resume_file not found: {resume_file}");
        }
    }

    let mut hunks: Vec<GrepContextHunk> = Vec::new();
    let mut used_chars = 0usize;
    let mut truncated = false;
    let mut truncation: Option<GrepContextTruncation> = None;
    let mut scanned_files = 0usize;
    let mut matched_files = 0usize;
    let mut returned_matches = 0usize;
    let mut total_matches = 0usize;
    let mut next_cursor_state: Option<(String, usize)> = None;

    let mut started = resume_file.is_none();
    'outer_files: for (display_file, file_path) in candidates {
        if !started {
            if Some(display_file.as_str()) != resume_file {
                continue;
            }
            started = true;
        }

        let file_resume_line = if Some(display_file.as_str()) == resume_file {
            resume_line
        } else {
            1
        };

        scanned_files += 1;

        let meta = match std::fs::metadata(&file_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }

        let file = match std::fs::File::open(&file_path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let mut reader = BufReader::new(file);

        let mut line = String::new();
        let mut line_no = 0usize;
        let mut match_lines: Vec<usize> = Vec::new();
        let mut stop_after_this_file = false;

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }

            line_no += 1;
            let text = line.trim_end_matches(&['\r', '\n'][..]);
            if !regex.is_match(text) {
                continue;
            }
            match_lines.push(line_no);
            if line_no >= file_resume_line {
                total_matches += 1;
                if total_matches >= max_matches {
                    truncated = true;
                    truncation = Some(GrepContextTruncation::Matches);
                    stop_after_this_file = true;
                    break;
                }
            }
        }

        if match_lines.is_empty() {
            continue;
        }
        matched_files += 1;

        let ranges: Vec<GrepRange> = match_lines
            .iter()
            .map(|&ln| {
                let start_line = ln.saturating_sub(before).max(1);
                let end_line = ln.saturating_add(after);
                GrepRange {
                    start_line,
                    end_line,
                    match_lines: vec![ln],
                }
            })
            .collect();
        let ranges = merge_grep_ranges(ranges);

        let file = match std::fs::File::open(&file_path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut line_no = 0usize;
        let mut range_idx = 0usize;

        while range_idx < ranges.len() {
            let range = &ranges[range_idx];
            let range_start_line = range.start_line.max(file_resume_line);
            if range_start_line > range.end_line {
                range_idx += 1;
                continue;
            }

            if hunks.len() >= max_hunks {
                truncated = true;
                truncation = Some(GrepContextTruncation::Hunks);
                next_cursor_state = Some((display_file.clone(), range_start_line));
                break 'outer_files;
            }

            let mut content = String::new();
            let mut end_line = range_start_line.saturating_sub(1);
            let mut stop_due_to_budget = false;

            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
                line_no += 1;

                if line_no < range_start_line {
                    continue;
                }
                if line_no > range.end_line {
                    break;
                }

                let text = line.trim_end_matches(&['\r', '\n'][..]);
                let line_chars = text.chars().count();
                let extra_chars = if content.is_empty() {
                    line_chars
                } else {
                    1 + line_chars
                };

                if used_chars.saturating_add(extra_chars) > max_chars {
                    truncated = true;
                    truncation = Some(GrepContextTruncation::Chars);
                    stop_due_to_budget = true;
                    break;
                }

                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(text);
                used_chars += extra_chars;
                end_line = line_no;
            }

            if stop_due_to_budget && content.is_empty() {
                break 'outer_files;
            }

            let mut match_lines = ranges[range_idx].match_lines.clone();
            match_lines.retain(|&ln| ln >= range_start_line && ln <= end_line);
            returned_matches += match_lines.len();

            hunks.push(GrepContextHunk {
                file: display_file.clone(),
                start_line: range_start_line,
                end_line,
                match_lines,
                content,
            });

            if stop_due_to_budget {
                next_cursor_state = Some((display_file.clone(), end_line.saturating_add(1)));
                break 'outer_files;
            }

            range_idx += 1;
        }

        if stop_after_this_file {
            break 'outer_files;
        }
    }

    let next_cursor = match next_cursor_state {
        Some((resume_file, resume_line)) => Some(encode_cursor(&GrepContextCursorV1 {
            v: CURSOR_VERSION,
            tool: "grep_context".to_string(),
            root: root_display.to_string(),
            pattern: request.pattern.clone(),
            file: request
                .file
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            file_pattern: file_pattern.map(str::to_string),
            case_sensitive,
            before,
            after,
            resume_file,
            resume_line,
        })?),
        None => None,
    };

    let result = GrepContextResult {
        pattern: request.pattern.clone(),
        source,
        file: request.file.clone(),
        file_pattern: request.file_pattern.clone(),
        case_sensitive,
        before,
        after,
        scanned_files,
        matched_files,
        returned_matches,
        returned_hunks: hunks.len(),
        used_chars,
        max_chars,
        truncated,
        truncation,
        next_cursor,
        hunks,
    };

    Ok(result)
}

// ============================================================================
// MCP Engine Cache (per-project, long-lived)
// ============================================================================

const ENGINE_CACHE_CAPACITY: usize = 4;

type EngineHandle = Arc<Mutex<EngineSlot>>;

struct ServiceState {
    engines: Mutex<EngineCache>,
}

impl ServiceState {
    fn new() -> Self {
        Self {
            engines: Mutex::new(EngineCache::new(ENGINE_CACHE_CAPACITY)),
        }
    }

    async fn engine_handle(&self, root: &Path) -> EngineHandle {
        let mut cache = self.engines.lock().await;
        cache.get_or_insert(root.to_path_buf())
    }
}

struct EngineCache {
    capacity: usize,
    entries: HashMap<PathBuf, EngineHandle>,
    order: VecDeque<PathBuf>,
}

impl EngineCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get_or_insert(&mut self, root: PathBuf) -> EngineHandle {
        if let Some(handle) = self.entries.get(&root).cloned() {
            self.touch(&root);
            return handle;
        }

        let handle = Arc::new(Mutex::new(EngineSlot { engine: None }));
        self.entries.insert(root.clone(), handle.clone());
        self.touch(&root);

        while self.entries.len() > self.capacity {
            let Some(evict_root) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&evict_root);
        }

        handle
    }

    fn touch(&mut self, root: &PathBuf) {
        if let Some(pos) = self.order.iter().position(|p| p == root) {
            self.order.remove(pos);
        }
        self.order.push_back(root.clone());
    }
}

struct EngineSlot {
    engine: Option<ProjectEngine>,
}

struct EngineLock {
    slot: tokio::sync::OwnedMutexGuard<EngineSlot>,
}

impl EngineLock {
    fn engine_mut(&mut self) -> &mut ProjectEngine {
        self.slot.engine.as_mut().expect("engine must be available")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EngineSignature {
    corpus_mtime_ms: Option<u64>,
    index_mtimes_ms: Vec<(String, Option<u64>)>,
}

struct ProjectEngine {
    signature: EngineSignature,
    root: PathBuf,
    context_search: MultiModelContextSearch,
    chunk_lookup: HashMap<String, usize>,
    available_models: Vec<String>,
    canonical_index_mtime: SystemTime,
    graph_language: Option<GraphLanguage>,
}

impl ProjectEngine {
    async fn ensure_graph(&mut self, language: GraphLanguage) -> Result<()> {
        if self.graph_language == Some(language) && self.context_search.assembler().is_some() {
            return Ok(());
        }

        let cache = GraphCache::new(&self.root);
        match cache
            .load(
                self.canonical_index_mtime,
                language,
                self.context_search.hybrid().chunks(),
                &self.chunk_lookup,
            )
            .await
        {
            Ok(Some(assembler)) => {
                self.context_search.set_assembler(assembler);
                self.graph_language = Some(language);
                return Ok(());
            }
            Ok(None) => {}
            Err(err) => log::warn!("Graph cache load error: {err:#}"),
        }

        self.context_search.build_graph(language)?;
        self.graph_language = Some(language);

        if let Some(assembler) = self.context_search.assembler() {
            if let Err(err) = cache
                .save(self.canonical_index_mtime, language, assembler)
                .await
            {
                log::warn!("Graph cache save error: {err:#}");
            }
        }

        Ok(())
    }
}

#[derive(Clone)]
struct GraphCache {
    path: PathBuf,
}

impl GraphCache {
    fn new(project_root: &Path) -> Self {
        Self {
            path: project_root
                .join(".context-finder")
                .join("graph_cache.json"),
        }
    }

    async fn load(
        &self,
        store_mtime: SystemTime,
        language: GraphLanguage,
        chunks: &[context_code_chunker::CodeChunk],
        chunk_index: &HashMap<String, usize>,
    ) -> Result<Option<ContextAssembler>> {
        if !self.path.exists() {
            return Ok(None);
        }

        let data = match tokio::fs::read(&self.path).await {
            Ok(bytes) => bytes,
            Err(err) => {
                log::warn!("Failed to read graph cache {}: {err}", self.path.display());
                return Ok(None);
            }
        };

        let cached: CachedGraph = match serde_json::from_slice(&data) {
            Ok(cached) => cached,
            Err(err) => {
                log::warn!("Graph cache corrupted ({}): {err}", self.path.display());
                return Ok(None);
            }
        };

        if cached.language != language {
            return Ok(None);
        }

        if cached.index_mtime_ms != unix_ms(store_mtime) {
            return Ok(None);
        }

        let mut graph = CodeGraph::new();
        let mut node_indices = Vec::new();

        for node in cached.nodes {
            let Some(&idx) = chunk_index.get(&node.chunk_id) else {
                return Ok(None);
            };
            let Some(chunk) = chunks.get(idx) else {
                return Ok(None);
            };

            let graph_node = GraphNode {
                symbol: node.symbol,
                chunk_id: node.chunk_id,
                chunk: Some(chunk.clone()),
            };
            let idx = graph.add_node(graph_node);
            node_indices.push(idx);
        }

        for edge in cached.edges {
            let Some(&from_idx) = node_indices.get(edge.from) else {
                return Ok(None);
            };
            let Some(&to_idx) = node_indices.get(edge.to) else {
                return Ok(None);
            };
            graph.add_edge(
                from_idx,
                to_idx,
                GraphEdge {
                    relationship: edge.relationship,
                    weight: edge.weight,
                },
            );
        }

        Ok(Some(ContextAssembler::new(graph)))
    }

    async fn save(
        &self,
        store_mtime: SystemTime,
        language: GraphLanguage,
        assembler: &ContextAssembler,
    ) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let cached = CachedGraph::from_assembler(store_mtime, language, assembler);
        let data = serde_json::to_vec_pretty(&cached)?;
        tokio::fs::write(&self.path, data)
            .await
            .with_context(|| format!("Failed to write graph cache {}", self.path.display()))
    }
}

#[derive(Serialize, Deserialize)]
struct CachedGraph {
    index_mtime_ms: u64,
    language: GraphLanguage,
    nodes: Vec<CachedNode>,
    edges: Vec<CachedEdge>,
}

#[derive(Serialize, Deserialize)]
struct CachedNode {
    symbol: Symbol,
    chunk_id: String,
}

#[derive(Serialize, Deserialize)]
struct CachedEdge {
    from: usize,
    to: usize,
    relationship: RelationshipType,
    weight: f32,
}

impl CachedGraph {
    fn from_assembler(
        store_mtime: SystemTime,
        language: GraphLanguage,
        assembler: &ContextAssembler,
    ) -> Self {
        let graph = assembler.graph();
        let mut node_map = HashMap::new();
        let mut nodes = Vec::new();

        for (idx, node) in graph.graph.node_indices().enumerate() {
            if let Some(data) = graph.graph.node_weight(node) {
                node_map.insert(node, idx);
                nodes.push(CachedNode {
                    symbol: data.symbol.clone(),
                    chunk_id: data.chunk_id.clone(),
                });
            }
        }

        let mut edges = Vec::new();
        for edge_id in graph.graph.edge_indices() {
            let Some((source, target)) = graph.graph.edge_endpoints(edge_id) else {
                continue;
            };
            let Some(weight) = graph.graph.edge_weight(edge_id) else {
                continue;
            };
            let (Some(&from), Some(&to)) = (node_map.get(&source), node_map.get(&target)) else {
                continue;
            };
            edges.push(CachedEdge {
                from,
                to,
                relationship: weight.relationship,
                weight: weight.weight,
            });
        }

        Self {
            index_mtime_ms: unix_ms(store_mtime),
            language,
            nodes,
            edges,
        }
    }
}

async fn compute_engine_signature(root: &Path, profile: &SearchProfile) -> Result<EngineSignature> {
    let corpus_path = corpus_path_for_project_root(root);
    let corpus_mtime_ms = match tokio::fs::metadata(&corpus_path)
        .await
        .and_then(|m| m.modified())
    {
        Ok(t) => Some(unix_ms(t)),
        Err(_) => None,
    };

    let default_model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    let mut models = Vec::new();
    models.push(default_model_id);
    models.extend(semantic_model_roster(profile));
    models.sort();
    models.dedup();

    let mut index_mtimes_ms = Vec::with_capacity(models.len());
    for model_id in models {
        let index_path = index_path_for_model(root, &model_id);
        let mtime_ms = match tokio::fs::metadata(&index_path)
            .await
            .and_then(|m| m.modified())
        {
            Ok(t) => Some(unix_ms(t)),
            Err(_) => None,
        };
        index_mtimes_ms.push((model_id, mtime_ms));
    }

    Ok(EngineSignature {
        corpus_mtime_ms,
        index_mtimes_ms,
    })
}

async fn build_project_engine(
    root: &Path,
    profile: &SearchProfile,
    signature: EngineSignature,
) -> Result<ProjectEngine> {
    let sources = load_semantic_indexes(root, profile).await?;
    let mut available_models: Vec<String> = sources.iter().map(|(id, _)| id.clone()).collect();
    available_models.sort();

    let canonical_model_id = available_models
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("No semantic indices available"))?;
    let canonical_index_path = index_path_for_model(root, &canonical_model_id);
    let canonical_index_mtime = tokio::fs::metadata(&canonical_index_path)
        .await
        .with_context(|| format!("Failed to stat {}", canonical_index_path.display()))?
        .modified()
        .with_context(|| format!("Failed to read mtime {}", canonical_index_path.display()))?;

    let corpus = ContextFinderService::load_chunk_corpus(root).await?;
    let hybrid = match corpus {
        Some(corpus) => {
            MultiModelHybridSearch::from_env_with_corpus(sources, profile.clone(), corpus)
        }
        None => MultiModelHybridSearch::from_env(sources, profile.clone()),
    }?;

    let context_search = MultiModelContextSearch::new(hybrid)?;
    let chunk_lookup = build_chunk_lookup(context_search.hybrid().chunks());

    Ok(ProjectEngine {
        signature,
        root: root.to_path_buf(),
        context_search,
        chunk_lookup,
        available_models,
        canonical_index_mtime,
        graph_language: None,
    })
}

// ============================================================================
// Tool Input/Output Schemas
// ============================================================================

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MapRequest {
    /// Project directory path (defaults to current directory)
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Directory depth for aggregation (default: 2)
    #[schemars(description = "Directory depth for grouping (1-4)")]
    pub depth: Option<usize>,

    /// Maximum number of directories to return
    #[schemars(description = "Limit number of results")]
    pub limit: Option<usize>,

    /// Opaque cursor token to continue a previous response
    #[schemars(description = "Opaque cursor token to continue a previous map response")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MapCursorV1 {
    v: u32,
    tool: String,
    root: String,
    depth: usize,
    offset: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MapResult {
    /// Total files in project
    pub total_files: usize,
    /// Total code chunks indexed
    pub total_chunks: usize,
    /// Total lines of code
    pub total_lines: usize,
    /// Directory breakdown
    pub directories: Vec<DirectoryInfo>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct DirectoryInfo {
    /// Directory path
    pub path: String,
    /// Number of files
    pub files: usize,
    /// Number of chunks
    pub chunks: usize,
    /// Percentage of codebase
    pub coverage_pct: f32,
    /// Top symbols in this directory
    pub top_symbols: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TextSearchRequest {
    /// Text pattern to search for (literal)
    #[schemars(description = "Text pattern to search for (literal substring)")]
    pub pattern: String,

    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Optional path filter (simple glob: '*' and '?' supported). If no glob metachars are
    /// present, treated as substring match against the relative file path.
    #[schemars(description = "Optional file path filter (glob or substring)")]
    pub file_pattern: Option<String>,

    /// Maximum number of matches to return (bounded)
    #[schemars(description = "Maximum number of matches to return (bounded)")]
    pub max_results: Option<usize>,

    /// Case-sensitive search (default: true)
    #[schemars(description = "Whether search is case-sensitive")]
    pub case_sensitive: Option<bool>,

    /// Whole-word match for identifier-like patterns (default: false)
    #[schemars(description = "If true, enforce identifier-like word boundaries")]
    pub whole_word: Option<bool>,

    /// Opaque cursor token to continue a previous response
    #[schemars(description = "Opaque cursor token to continue a previous text_search response")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum TextSearchCursorModeV1 {
    Corpus {
        file_index: usize,
        chunk_index: usize,
        line_offset: usize,
    },
    Filesystem {
        file_index: usize,
        line_offset: usize,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct TextSearchCursorV1 {
    v: u32,
    tool: String,
    root: String,
    pattern: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_pattern: Option<String>,
    case_sensitive: bool,
    whole_word: bool,
    #[serde(flatten)]
    mode: TextSearchCursorModeV1,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TextSearchResult {
    pub pattern: String,
    pub source: String,
    pub scanned_files: usize,
    pub matched_files: usize,
    pub skipped_large_files: usize,
    pub returned: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub matches: Vec<TextSearchMatch>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TextSearchMatch {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FileSliceRequest {
    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// File path (relative to project root)
    #[schemars(description = "File path (relative to project root)")]
    pub file: String,

    /// First line to include (1-based, default: 1)
    #[schemars(description = "First line to include (1-based)")]
    pub start_line: Option<usize>,

    /// Maximum number of lines to return (default: 200)
    #[schemars(description = "Maximum number of lines to return (bounded)")]
    pub max_lines: Option<usize>,

    /// Maximum number of UTF-8 characters for the returned slice (default: 20000)
    #[schemars(description = "Maximum number of UTF-8 characters for the returned slice")]
    pub max_chars: Option<usize>,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileSliceTruncation {
    MaxLines,
    MaxChars,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FileSliceResult {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub returned_lines: usize,
    pub used_chars: usize,
    pub max_lines: usize,
    pub max_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<FileSliceTruncation>,
    pub file_size_bytes: u64,
    pub file_mtime_ms: u64,
    pub content_sha256: String,
    pub content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListFilesRequest {
    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Optional file path filter (simple glob: '*' and '?' supported). If no glob metachars are
    /// present, treated as substring match against the relative file path.
    #[schemars(description = "Optional file path filter (glob or substring)")]
    pub file_pattern: Option<String>,

    /// Maximum number of files to return (default: 200)
    #[schemars(description = "Maximum number of file paths to return (bounded)")]
    pub limit: Option<usize>,

    /// Maximum number of UTF-8 characters across returned file paths (default: 20000)
    #[schemars(description = "Maximum number of UTF-8 characters across returned file paths")]
    pub max_chars: Option<usize>,

    /// Opaque cursor token to continue a previous response
    #[schemars(description = "Opaque cursor token to continue a previous list_files response")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ListFilesCursorV1 {
    v: u32,
    tool: String,
    root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_pattern: Option<String>,
    last_file: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ListFilesTruncation {
    Limit,
    MaxChars,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListFilesResult {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_pattern: Option<String>,
    pub scanned_files: usize,
    pub returned: usize,
    pub used_chars: usize,
    pub limit: usize,
    pub max_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<ListFilesTruncation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub files: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GrepContextRequest {
    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Regex pattern (Rust regex syntax)
    #[schemars(description = "Regex pattern to search for (Rust regex syntax)")]
    pub pattern: String,

    /// Optional single file path (relative to project root)
    #[schemars(description = "Optional single file path (relative to project root)")]
    pub file: Option<String>,

    /// Optional file path filter (simple glob: '*' and '?' supported). If no glob metachars are
    /// present, treated as substring match against the relative file path.
    #[schemars(description = "Optional file path filter (glob or substring)")]
    pub file_pattern: Option<String>,

    /// Symmetric context lines before and after each match (grep -C)
    #[schemars(description = "Symmetric context lines before and after each match")]
    pub context: Option<usize>,

    /// Number of lines before each match (grep -B)
    #[schemars(description = "Number of lines before each match")]
    pub before: Option<usize>,

    /// Number of lines after each match (grep -A)
    #[schemars(description = "Number of lines after each match")]
    pub after: Option<usize>,

    /// Maximum number of matching lines to process (bounded)
    #[schemars(description = "Maximum number of matching lines to process (bounded)")]
    pub max_matches: Option<usize>,

    /// Maximum number of hunks to return (bounded)
    #[schemars(description = "Maximum number of hunks to return (bounded)")]
    pub max_hunks: Option<usize>,

    /// Maximum number of UTF-8 characters across returned hunks (default: 20000)
    #[schemars(description = "Maximum number of UTF-8 characters across returned hunks")]
    pub max_chars: Option<usize>,

    /// Case-sensitive regex matching (default: true)
    #[schemars(description = "Whether regex matching is case-sensitive")]
    pub case_sensitive: Option<bool>,

    /// Opaque cursor token to continue a previous response
    #[schemars(description = "Opaque cursor token to continue a previous grep_context response")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GrepContextCursorV1 {
    v: u32,
    tool: String,
    root: String,
    pattern: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_pattern: Option<String>,
    case_sensitive: bool,
    before: usize,
    after: usize,
    resume_file: String,
    resume_line: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GrepContextTruncation {
    #[serde(rename = "max_chars")]
    Chars,
    #[serde(rename = "max_matches")]
    Matches,
    #[serde(rename = "max_hunks")]
    Hunks,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GrepContextHunk {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub match_lines: Vec<usize>,
    pub content: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GrepContextResult {
    pub pattern: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_pattern: Option<String>,
    pub case_sensitive: bool,
    pub before: usize,
    pub after: usize,
    pub scanned_files: usize,
    pub matched_files: usize,
    pub returned_matches: usize,
    pub returned_hunks: usize,
    pub used_chars: usize,
    pub max_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<GrepContextTruncation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub hunks: Vec<GrepContextHunk>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RepoOnboardingPackRequest {
    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Directory depth for aggregation (default: 2)
    #[schemars(description = "Directory depth for grouping (1-4)")]
    pub map_depth: Option<usize>,

    /// Maximum number of directories to return (default: 20)
    #[schemars(description = "Limit number of map nodes returned")]
    pub map_limit: Option<usize>,

    /// Optional explicit doc file paths to include (relative to project root). If omitted, uses a
    /// built-in prioritized list (README/AGENTS/docs/...).
    #[schemars(
        description = "Optional explicit doc file paths to include (relative to project root)"
    )]
    pub doc_paths: Option<Vec<String>>,

    /// Maximum number of docs to include (default: 8)
    #[schemars(description = "Maximum number of docs to include (bounded)")]
    pub docs_limit: Option<usize>,

    /// Max lines per doc slice (default: 200)
    #[schemars(description = "Max lines per doc slice")]
    pub doc_max_lines: Option<usize>,

    /// Max chars per doc slice (default: 6000)
    #[schemars(description = "Max UTF-8 chars per doc slice")]
    pub doc_max_chars: Option<usize>,

    /// Maximum number of UTF-8 characters for the entire onboarding pack (default: 20000)
    #[schemars(description = "Maximum number of UTF-8 characters for the onboarding pack")]
    pub max_chars: Option<usize>,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepoOnboardingPackTruncation {
    MaxChars,
    DocsLimit,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RepoOnboardingPackBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<RepoOnboardingPackTruncation>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RepoOnboardingNextAction {
    pub tool: String,
    pub args: serde_json::Value,
    pub reason: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RepoOnboardingPackResult {
    pub version: u32,
    pub root: String,
    pub map: MapResult,
    pub docs: Vec<FileSliceResult>,
    pub next_actions: Vec<RepoOnboardingNextAction>,
    pub budget: RepoOnboardingPackBudget,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DoctorRequest {
    /// Project directory path (optional)
    #[schemars(description = "Project directory path (optional)")]
    pub path: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DoctorResult {
    pub env: DoctorEnvResult,
    pub project: Option<DoctorProjectResult>,
    pub issues: Vec<String>,
    pub hints: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DoctorEnvResult {
    pub profile: String,
    pub model_dir: String,
    pub model_manifest_exists: bool,
    pub models: Vec<DoctorModelStatus>,
    pub gpu: runtime_env::GpuEnvReport,
    pub cuda_disabled: bool,
    pub allow_cpu_fallback: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DoctorModelStatus {
    pub id: String,
    pub installed: bool,
    pub missing_assets: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DoctorIndexDrift {
    pub model: String,
    pub index_path: String,
    pub index_chunks: usize,
    pub corpus_chunks: usize,
    pub missing_chunks: usize,
    pub extra_chunks: usize,
    pub missing_file_samples: Vec<String>,
    pub extra_file_samples: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DoctorProjectResult {
    pub root: String,
    pub corpus_path: String,
    pub has_corpus: bool,
    pub indexed_models: Vec<String>,
    pub drift: Vec<DoctorIndexDrift>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatchToolName {
    Map,
    FileSlice,
    ListFiles,
    TextSearch,
    GrepContext,
    Doctor,
    Search,
    Context,
    ContextPack,
    Index,
    Impact,
    Trace,
    Explain,
    Overview,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchRequest {
    /// Batch schema version (default: 1)
    #[schemars(description = "Batch schema version (default: 1)")]
    pub version: Option<u32>,

    /// Project directory path (defaults to current directory)
    #[schemars(description = "Project directory path (defaults to current directory)")]
    pub path: Option<String>,

    /// Maximum number of UTF-8 characters for the serialized batch result (best effort).
    #[schemars(
        description = "Maximum number of UTF-8 characters for the serialized batch result (best effort)."
    )]
    pub max_chars: Option<usize>,

    /// If true, stop processing after the first item error.
    #[schemars(description = "If true, stop processing after the first item error.")]
    #[serde(default)]
    pub stop_on_error: bool,

    /// Batch items to execute.
    #[schemars(description = "Batch items to execute.")]
    pub items: Vec<BatchItem>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchItem {
    /// Caller-provided identifier used to correlate results.
    pub id: String,

    /// Tool name to execute.
    pub tool: BatchToolName,

    /// Tool input object (tool-specific). Defaults to `{}`.
    #[serde(default)]
    pub input: serde_json::Value,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatchItemStatus {
    Ok,
    Error,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct BatchBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct BatchItemResult {
    pub id: String,
    pub tool: BatchToolName,
    pub status: BatchItemStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub data: serde_json::Value,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct BatchResult {
    pub version: u32,
    pub items: Vec<BatchItemResult>,
    pub budget: BatchBudget,
}

#[derive(Debug, Deserialize)]
struct ModelManifestFile {
    models: Vec<ModelManifestModel>,
}

#[derive(Debug, Deserialize)]
struct ModelManifestModel {
    id: String,
    assets: Vec<ModelManifestAsset>,
}

#[derive(Debug, Deserialize)]
struct ModelManifestAsset {
    path: String,
}

fn validate_relative_model_asset_path(path: &Path) -> Result<()> {
    let mut has_component = false;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                anyhow::bail!("asset path must be relative");
            }
            Component::ParentDir => {
                anyhow::bail!("asset path must not contain '..'");
            }
            Component::CurDir => {}
            Component::Normal(_) => {
                has_component = true;
            }
        }
    }
    if !has_component {
        anyhow::bail!("asset path is empty");
    }
    Ok(())
}

fn safe_join_model_asset_path(model_dir: &Path, asset_path: &str) -> Result<PathBuf> {
    let rel = Path::new(asset_path);
    validate_relative_model_asset_path(rel)
        .with_context(|| format!("Invalid model asset path '{asset_path}'"))?;
    Ok(model_dir.join(rel))
}

#[cfg(test)]
mod model_asset_path_tests {
    use super::*;

    #[test]
    fn safe_join_rejects_traversal_and_absolute_paths() {
        let base = Path::new("models");
        assert!(safe_join_model_asset_path(base, "../escape").is_err());
        assert!(safe_join_model_asset_path(base, "m1/../escape").is_err());
        assert!(safe_join_model_asset_path(base, "").is_err());

        #[cfg(unix)]
        assert!(safe_join_model_asset_path(base, "/etc/passwd").is_err());
    }

    #[test]
    fn safe_join_accepts_normal_relative_paths() {
        let base = Path::new("models");
        let path = safe_join_model_asset_path(base, "m1/model.onnx").expect("valid path");
        assert!(path.starts_with(base));
    }
}

#[derive(Debug, Deserialize)]
struct IndexIdMapOnly {
    #[serde(default)]
    schema_version: Option<u32>,
    #[serde(default)]
    id_map: HashMap<usize, String>,
}

async fn load_model_statuses(model_dir: &Path) -> Result<(bool, Vec<DoctorModelStatus>)> {
    let manifest_path = model_dir.join("manifest.json");
    if !manifest_path.exists() {
        return Ok((false, Vec::new()));
    }

    let bytes = tokio::fs::read(&manifest_path)
        .await
        .with_context(|| format!("Failed to read model manifest {}", manifest_path.display()))?;
    let parsed: ModelManifestFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("Failed to parse model manifest {}", manifest_path.display()))?;

    let mut statuses = Vec::new();
    for model in parsed.models {
        let mut missing = Vec::new();
        for asset in model.assets {
            let full = match safe_join_model_asset_path(model_dir, &asset.path) {
                Ok(path) => path,
                Err(err) => {
                    missing.push(format!("invalid_path: {} ({err})", asset.path));
                    continue;
                }
            };
            if !full.exists() {
                missing.push(asset.path);
            }
        }
        let installed = missing.is_empty();
        statuses.push(DoctorModelStatus {
            id: model.id,
            installed,
            missing_assets: missing,
        });
    }
    Ok((true, statuses))
}

async fn load_corpus_chunk_ids(corpus_path: &Path) -> Result<HashSet<String>> {
    let corpus = ChunkCorpus::load(corpus_path).await?;
    let mut ids = HashSet::new();
    for chunks in corpus.files().values() {
        for chunk in chunks {
            ids.insert(format!(
                "{}:{}:{}",
                chunk.file_path, chunk.start_line, chunk.end_line
            ));
        }
    }
    Ok(ids)
}

async fn load_index_chunk_ids(index_path: &Path) -> Result<HashSet<String>> {
    let bytes = tokio::fs::read(index_path)
        .await
        .with_context(|| format!("Failed to read index {}", index_path.display()))?;
    let parsed: IndexIdMapOnly = serde_json::from_slice(&bytes)
        .with_context(|| format!("Failed to parse index {}", index_path.display()))?;
    // schema_version is tracked for diagnostics, but chunk id extraction relies on id_map values.
    let _ = parsed.schema_version.unwrap_or(1);
    Ok(parsed.id_map.into_values().collect())
}

fn chunk_id_file_path(chunk_id: &str) -> Option<String> {
    let mut parts = chunk_id.rsplitn(3, ':');
    let _end = parts.next()?;
    let _start = parts.next()?;
    Some(parts.next()?.to_string())
}

fn sample_file_paths<'a, I>(chunk_ids: I, limit: usize) -> Vec<String>
where
    I: Iterator<Item = &'a String>,
{
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for id in chunk_ids {
        if out.len() >= limit {
            break;
        }
        let Some(file) = chunk_id_file_path(id) else {
            continue;
        };
        if seen.insert(file.clone()) {
            out.push(file);
        }
    }
    out
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchRequest {
    /// Search query (semantic search)
    #[schemars(description = "Natural language search query")]
    pub query: String,

    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Maximum results (default: 10)
    #[schemars(description = "Maximum number of results (1-50)")]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SearchResult {
    /// File path
    pub file: String,
    /// Start line
    pub start_line: usize,
    /// End line
    pub end_line: usize,
    /// Symbol name (if any)
    pub symbol: Option<String>,
    /// Symbol type (function, struct, etc.)
    pub symbol_type: Option<String>,
    /// Relevance score (0-1)
    pub score: f32,
    /// Code content
    pub content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContextRequest {
    /// Search query
    #[schemars(description = "Natural language search query")]
    pub query: String,

    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Maximum primary results (default: 5)
    #[schemars(description = "Maximum number of primary results")]
    pub limit: Option<usize>,

    /// Search strategy: direct, extended, deep
    #[schemars(
        description = "Graph traversal depth: direct (none), extended (1-hop), deep (2-hop)"
    )]
    pub strategy: Option<String>,

    /// Graph language: rust, python, javascript, typescript
    #[schemars(description = "Programming language for graph analysis")]
    pub language: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContextPackRequest {
    /// Search query
    #[schemars(description = "Natural language search query")]
    pub query: String,

    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Maximum primary results (default: 10)
    #[schemars(description = "Maximum number of primary results")]
    pub limit: Option<usize>,

    /// Maximum total characters for packed output (default: 20000)
    #[schemars(description = "Maximum total characters in packed output")]
    pub max_chars: Option<usize>,

    /// Related chunks per primary (default: 3)
    #[schemars(description = "Maximum related chunks per primary")]
    pub max_related_per_primary: Option<usize>,

    /// Search strategy: direct, extended, deep
    #[schemars(
        description = "Graph traversal depth: direct (none), extended (1-hop), deep (2-hop)"
    )]
    pub strategy: Option<String>,

    /// Graph language: rust, python, javascript, typescript
    #[schemars(description = "Programming language for graph analysis")]
    pub language: Option<String>,

    /// Auto-index the project if missing (opt-in)
    #[schemars(
        description = "If true, automatically index the project (single-model) when missing before building the context pack"
    )]
    pub auto_index: Option<bool>,

    /// Include debug output (adds a second MCP content block with debug JSON)
    #[schemars(description = "Include debug output as an additional response block")]
    pub trace: Option<bool>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ContextResult {
    /// Primary search results
    pub results: Vec<ContextHit>,
    /// Total related code found
    pub related_count: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ContextHit {
    /// File path
    pub file: String,
    /// Start line
    pub start_line: usize,
    /// End line
    pub end_line: usize,
    /// Symbol name
    pub symbol: Option<String>,
    /// Relevance score
    pub score: f32,
    /// Code content
    pub content: String,
    /// Related code through graph
    pub related: Vec<RelatedCode>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RelatedCode {
    /// File path
    pub file: String,
    /// Line range as string
    pub lines: String,
    /// Symbol name
    pub symbol: Option<String>,
    /// Relationship path (e.g., "Calls", "Uses -> Uses")
    pub relationship: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IndexRequest {
    /// Project directory path
    #[schemars(description = "Project directory to index")]
    pub path: Option<String>,

    /// Force full reindex (alias for `full`)
    #[schemars(description = "Force full reindex ignoring cache (alias for `full`)")]
    pub force: Option<bool>,

    /// Index expert roster models from the active profile (opt-in)
    #[schemars(
        description = "If true, index the profile's expert roster models in addition to the primary model"
    )]
    pub experts: Option<bool>,

    /// Additional model IDs to index (opt-in)
    #[schemars(description = "Additional embedding model IDs to index")]
    pub models: Option<Vec<String>>,

    /// Full reindex (skip incremental checks)
    #[schemars(description = "Run a full reindex (skip incremental checks)")]
    pub full: Option<bool>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct IndexResult {
    /// Number of files indexed
    pub files: usize,
    /// Number of chunks created
    pub chunks: usize,
    /// Indexing time in milliseconds
    pub time_ms: u64,
    /// Index file path
    pub index_path: String,
}

// ============================================================================
// New Tool Schemas: impact, trace, explain, overview
// ============================================================================

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ImpactRequest {
    /// Symbol name to analyze
    #[schemars(description = "Symbol name to find usages of (e.g., 'VectorStore', 'search')")]
    pub symbol: String,

    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Depth of transitive usages (1=direct, 2=transitive)
    #[schemars(description = "Depth for transitive impact analysis (1-3)")]
    pub depth: Option<usize>,

    /// Programming language
    #[schemars(description = "Programming language: rust, python, javascript, typescript")]
    pub language: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ImpactResult {
    /// Symbol that was analyzed
    pub symbol: String,
    /// Definition location
    pub definition: Option<SymbolLocation>,
    /// Total usage count
    pub total_usages: usize,
    /// Number of files affected
    pub files_affected: usize,
    /// Direct usages
    pub direct: Vec<UsageInfo>,
    /// Transitive usages (if depth > 1)
    pub transitive: Vec<UsageInfo>,
    /// Related tests
    pub tests: Vec<String>,
    /// Is part of public API
    pub public_api: bool,
    /// Mermaid diagram
    pub mermaid: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SymbolLocation {
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct UsageInfo {
    pub file: String,
    pub line: usize,
    pub symbol: String,
    pub relationship: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TraceRequest {
    /// Start symbol
    #[schemars(description = "Starting symbol name")]
    pub from: String,

    /// End symbol
    #[schemars(description = "Target symbol name")]
    pub to: String,

    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Programming language
    #[schemars(description = "Programming language: rust, python, javascript, typescript")]
    pub language: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TraceResult {
    /// Whether path was found
    pub found: bool,
    /// Call chain path
    pub path: Vec<TraceStep>,
    /// Path depth
    pub depth: usize,
    /// Mermaid sequence diagram
    pub mermaid: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TraceStep {
    /// Symbol name
    pub symbol: String,
    /// File path
    pub file: String,
    /// Line number
    pub line: usize,
    /// Relationship to next step
    pub relationship: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExplainRequest {
    /// Symbol name to explain
    #[schemars(description = "Symbol name to get detailed information about")]
    pub symbol: String,

    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Programming language
    #[schemars(description = "Programming language: rust, python, javascript, typescript")]
    pub language: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExplainResult {
    /// Symbol name
    pub symbol: String,
    /// Symbol kind (function, struct, etc.)
    pub kind: String,
    /// File path
    pub file: String,
    /// Line number
    pub line: usize,
    /// Documentation (if available)
    pub documentation: Option<String>,
    /// Dependencies (what this symbol uses/calls)
    pub dependencies: Vec<String>,
    /// Dependents (what uses/calls this symbol)
    pub dependents: Vec<String>,
    /// Related tests
    pub tests: Vec<String>,
    /// Code content
    pub content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OverviewRequest {
    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Programming language
    #[schemars(description = "Programming language: rust, python, javascript, typescript")]
    pub language: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct OverviewResult {
    /// Project info
    pub project: ProjectInfo,
    /// Architecture layers
    pub layers: Vec<LayerInfo>,
    /// Entry points
    pub entry_points: Vec<String>,
    /// Key types (most connected)
    pub key_types: Vec<KeyTypeInfo>,
    /// Graph statistics
    pub graph_stats: GraphStats,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ProjectInfo {
    pub name: String,
    pub files: usize,
    pub chunks: usize,
    pub lines: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LayerInfo {
    pub name: String,
    pub files: usize,
    pub role: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct KeyTypeInfo {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub coupling: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GraphStats {
    pub nodes: usize,
    pub edges: usize,
}

// ============================================================================
// Tool Implementations
// ============================================================================

#[tool_router]
impl ContextFinderService {
    /// Get project structure overview
    #[tool(
        description = "Get project structure overview with directories, files, and top symbols. Use this first to understand a new codebase."
    )]
    pub async fn map(
        &self,
        Parameters(request): Parameters<MapRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let depth = request.depth.unwrap_or(2).clamp(1, 4);
        let limit = request.limit.unwrap_or(10);

        let root = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid path: {e}"
                ))]));
            }
        };
        self.touch_daemon_best_effort(&root);
        let root_display = root.to_string_lossy().to_string();

        let mut offset = 0usize;
        if let Some(cursor) = request
            .cursor
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let decoded: MapCursorV1 = match decode_cursor(cursor) {
                Ok(v) => v,
                Err(err) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid cursor: {err}"
                    ))]));
                }
            };
            if decoded.v != CURSOR_VERSION || decoded.tool != "map" {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: wrong tool",
                )]));
            }
            if decoded.root != root_display {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: different root",
                )]));
            }
            if decoded.depth != depth {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: different depth",
                )]));
            }
            offset = decoded.offset;
        }

        let result = match compute_map_result(&root, &root_display, depth, limit, offset).await {
            Ok(result) => result,
            Err(err) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {err:#}"
                ))]));
            }
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Repo onboarding pack (map + key docs slices + next actions).
    #[tool(
        description = "Build a repo onboarding pack: map + key docs (via file slices) + next actions. Returns a single bounded JSON response for fast project adoption."
    )]
    pub async fn repo_onboarding_pack(
        &self,
        Parameters(request): Parameters<RepoOnboardingPackRequest>,
    ) -> Result<CallToolResult, McpError> {
        const VERSION: u32 = 1;
        const DEFAULT_MAX_CHARS: usize = 20_000;
        const MAX_MAX_CHARS: usize = 500_000;
        const DEFAULT_MAP_DEPTH: usize = 2;
        const DEFAULT_MAP_LIMIT: usize = 20;
        const DEFAULT_DOCS_LIMIT: usize = 8;
        const MAX_DOCS_LIMIT: usize = 25;
        const DEFAULT_DOC_MAX_LINES: usize = 200;
        const MAX_DOC_MAX_LINES: usize = 5_000;
        const DEFAULT_DOC_MAX_CHARS: usize = 6_000;
        const MAX_DOC_MAX_CHARS: usize = 100_000;

        let root_path = PathBuf::from(request.path.as_deref().unwrap_or("."));
        let root = match root_path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid path: {e}"
                ))]));
            }
        };
        self.touch_daemon_best_effort(&root);

        let max_chars = request
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(1, MAX_MAX_CHARS);
        let map_depth = request.map_depth.unwrap_or(DEFAULT_MAP_DEPTH).clamp(1, 4);
        let map_limit = request.map_limit.unwrap_or(DEFAULT_MAP_LIMIT).clamp(1, 200);
        let docs_limit = request
            .docs_limit
            .unwrap_or(DEFAULT_DOCS_LIMIT)
            .clamp(0, MAX_DOCS_LIMIT);
        let doc_max_lines = request
            .doc_max_lines
            .unwrap_or(DEFAULT_DOC_MAX_LINES)
            .clamp(1, MAX_DOC_MAX_LINES);
        let doc_max_chars = request
            .doc_max_chars
            .unwrap_or(DEFAULT_DOC_MAX_CHARS)
            .clamp(1, MAX_DOC_MAX_CHARS);

        let root_display = root.to_string_lossy().to_string();

        let map = match compute_map_result(&root, &root_display, map_depth, map_limit, 0).await {
            Ok(result) => result,
            Err(err) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {err:#}"
                ))]));
            }
        };

        let has_corpus = match Self::load_chunk_corpus(&root).await {
            Ok(v) => v.is_some(),
            Err(_) => false,
        };

        let mut next_actions = Vec::new();
        if !has_corpus {
            next_actions.push(RepoOnboardingNextAction {
                tool: "index".to_string(),
                args: serde_json::json!({ "path": root_display.clone() }),
                reason:
                    "Build the semantic index (enables search/context/context_pack/impact/trace)."
                        .to_string(),
            });
        }
        next_actions.push(RepoOnboardingNextAction {
            tool: "grep_context".to_string(),
            args: serde_json::json!({
                "path": root_display.clone(),
                "pattern": "TODO|FIXME",
                "context": 10,
                "max_hunks": 50,
            }),
            reason: "Scan for TODO/FIXME across the repo with surrounding context hunks."
                .to_string(),
        });
        next_actions.push(RepoOnboardingNextAction {
            tool: "batch".to_string(),
            args: serde_json::json!({
                "version": 2,
                "path": root_display.clone(),
                "max_chars": 20000,
                "items": [
                    { "id": "docs", "tool": "list_files", "input": { "file_pattern": "*.md", "limit": 200 } },
                    { "id": "read", "tool": "file_slice", "input": { "file": { "$ref": "#/items/docs/data/files/0" }, "start_line": 1, "max_lines": 200 } }
                ]
            }),
            reason: "Example: chain tools in one call with `$ref` dependencies (batch v2).".to_string(),
        });
        if has_corpus {
            next_actions.push(RepoOnboardingNextAction {
                tool: "context_pack".to_string(),
                args: serde_json::json!({
                    "path": root_display.clone(),
                    "query": "describe what you want to change",
                    "strategy": "extended",
                    "max_chars": 20000,
                }),
                reason: "One-shot semantic onboarding pack for a concrete question.".to_string(),
            });
        }

        let default_doc_candidates = [
            "README.md",
            "docs/README.md",
            "docs/QUICK_START.md",
            "USAGE_EXAMPLES.md",
            "PHILOSOPHY.md",
            "AGENTS.md",
            "CONTRIBUTING.md",
            "docs/COMMAND_RFC.md",
        ];

        let mut seen = HashSet::new();
        let mut doc_candidates: Vec<String> = Vec::new();
        if let Some(custom) = request.doc_paths.as_ref() {
            for rel in custom {
                let rel = rel.trim();
                if rel.is_empty() {
                    continue;
                }
                let rel = rel.replace('\\', "/");
                if seen.insert(rel.clone()) {
                    doc_candidates.push(rel);
                }
            }
        } else {
            for rel in default_doc_candidates {
                if seen.insert(rel.to_string()) {
                    doc_candidates.push(rel.to_string());
                }
            }
        }

        let mut result = RepoOnboardingPackResult {
            version: VERSION,
            root: root_display.clone(),
            map,
            docs: Vec::new(),
            next_actions,
            budget: RepoOnboardingPackBudget {
                max_chars,
                used_chars: 0,
                truncated: false,
                truncation: None,
            },
        };

        let mut docs_included = 0usize;
        for (idx, rel) in doc_candidates.iter().enumerate() {
            if docs_included >= docs_limit {
                result.budget.truncated = true;
                result.budget.truncation = Some(RepoOnboardingPackTruncation::DocsLimit);
                break;
            }

            let slice =
                match compute_onboarding_doc_slice(&root, rel, 1, doc_max_lines, doc_max_chars) {
                    Ok(slice) => slice,
                    Err(_) => continue,
                };
            result.docs.push(slice);
            docs_included += 1;

            if let Err(err) = finalize_repo_onboarding_budget(&mut result) {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {err:#}"
                ))]));
            }
            if result.budget.used_chars > max_chars {
                result.docs.pop();
                result.budget.truncated = true;
                result.budget.truncation = Some(RepoOnboardingPackTruncation::MaxChars);
                let _ = finalize_repo_onboarding_budget(&mut result);
                break;
            }

            if idx + 1 >= doc_candidates.len() {
                break;
            }
        }

        if let Err(err) = finalize_repo_onboarding_budget(&mut result) {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: {err:#}"
            ))]));
        }
        while result.budget.used_chars > max_chars && !result.next_actions.is_empty() {
            result.next_actions.pop();
            result.budget.truncated = true;
            result.budget.truncation = Some(RepoOnboardingPackTruncation::MaxChars);
            let _ = finalize_repo_onboarding_budget(&mut result);
        }
        while result.budget.used_chars > max_chars && !result.docs.is_empty() {
            result.docs.pop();
            result.budget.truncated = true;
            result.budget.truncation = Some(RepoOnboardingPackTruncation::MaxChars);
            let _ = finalize_repo_onboarding_budget(&mut result);
        }
        if result.budget.used_chars > max_chars {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "max_chars={max_chars} is too small for the onboarding pack payload"
            ))]));
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Bounded exact text search (literal substring), as a safe `rg` replacement.
    #[tool(
        description = "Search for an exact text pattern in project files with bounded output (rg-like, but safe for agent context). Uses corpus if available, otherwise scans files without side effects."
    )]
    pub async fn text_search(
        &self,
        Parameters(request): Parameters<TextSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let root = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid path: {e}"
                ))]));
            }
        };
        self.touch_daemon_best_effort(&root);

        let pattern = request.pattern.trim();
        if pattern.is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "Pattern must not be empty",
            )]));
        }

        let file_pattern = request
            .file_pattern
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        let max_results = request.max_results.unwrap_or(50).clamp(1, 1000);
        let case_sensitive = request.case_sensitive.unwrap_or(true);
        let whole_word = request.whole_word.unwrap_or(false);
        let root_display = root.to_string_lossy().to_string();
        let normalized_file_pattern = file_pattern.map(str::to_string);

        let mut cursor_mode: Option<TextSearchCursorModeV1> = None;
        if let Some(cursor) = request
            .cursor
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let decoded: TextSearchCursorV1 = match decode_cursor(cursor) {
                Ok(v) => v,
                Err(err) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid cursor: {err}"
                    ))]));
                }
            };
            if decoded.v != CURSOR_VERSION || decoded.tool != "text_search" {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: wrong tool",
                )]));
            }
            if decoded.root != root_display {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: different root",
                )]));
            }
            if decoded.pattern != pattern {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: different pattern",
                )]));
            }
            if decoded.file_pattern != normalized_file_pattern {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: different file_pattern",
                )]));
            }
            if decoded.case_sensitive != case_sensitive || decoded.whole_word != whole_word {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: different search options",
                )]));
            }
            cursor_mode = Some(decoded.mode);
        }

        const MAX_FILE_BYTES: u64 = 2_000_000;

        let mut matches = Vec::new();
        let mut matched_files: HashSet<String> = HashSet::new();
        let mut scanned_files = 0usize;
        let mut skipped_large_files = 0usize;
        let mut truncated = false;
        let mut next_cursor: Option<String> = None;
        let source: String;

        let corpus = match Self::load_chunk_corpus(&root).await {
            Ok(corpus) => corpus,
            Err(err) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {err:#}"
                ))]));
            }
        };

        if let Some(corpus) = corpus {
            source = "corpus".to_string();

            let (start_file_index, start_chunk_index, start_line_offset) =
                match cursor_mode.as_ref() {
                    None => (0usize, 0usize, 0usize),
                    Some(TextSearchCursorModeV1::Corpus {
                        file_index,
                        chunk_index,
                        line_offset,
                    }) => (*file_index, *chunk_index, *line_offset),
                    Some(TextSearchCursorModeV1::Filesystem { .. }) => {
                        return Ok(CallToolResult::error(vec![Content::text(
                            "Invalid cursor: wrong mode",
                        )]));
                    }
                };

            let mut files: Vec<(&String, &Vec<context_code_chunker::CodeChunk>)> =
                corpus.files().iter().collect();
            files.sort_by(|a, b| a.0.cmp(b.0));
            files.retain(|(file, _)| Self::matches_file_pattern(file, file_pattern));

            if start_file_index > files.len() {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: out of range",
                )]));
            }

            let mut next_state: Option<TextSearchCursorModeV1> = None;

            'outer_corpus: for (file_index, (_file, chunks)) in
                files.iter().enumerate().skip(start_file_index)
            {
                if matches.len() >= max_results {
                    truncated = true;
                    next_state = Some(TextSearchCursorModeV1::Corpus {
                        file_index,
                        chunk_index: 0,
                        line_offset: 0,
                    });
                    break 'outer_corpus;
                }

                scanned_files += 1;

                let mut chunk_refs: Vec<&context_code_chunker::CodeChunk> = chunks.iter().collect();
                chunk_refs.sort_by(|a, b| {
                    a.start_line
                        .cmp(&b.start_line)
                        .then_with(|| a.end_line.cmp(&b.end_line))
                });

                let first_file = file_index == start_file_index;
                let start_chunk = if first_file { start_chunk_index } else { 0 };
                if start_chunk > chunk_refs.len() {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Invalid cursor: out of range",
                    )]));
                }

                for (chunk_index, chunk) in chunk_refs.iter().enumerate().skip(start_chunk) {
                    if matches.len() >= max_results {
                        truncated = true;
                        next_state = Some(TextSearchCursorModeV1::Corpus {
                            file_index,
                            chunk_index,
                            line_offset: 0,
                        });
                        break 'outer_corpus;
                    }

                    let line_start = if first_file && chunk_index == start_chunk {
                        start_line_offset
                    } else {
                        0
                    };

                    for (offset, line_text) in chunk.content.lines().enumerate().skip(line_start) {
                        if matches.len() >= max_results {
                            truncated = true;
                            next_state = Some(TextSearchCursorModeV1::Corpus {
                                file_index,
                                chunk_index,
                                line_offset: offset,
                            });
                            break 'outer_corpus;
                        }

                        let Some(col_byte) =
                            Self::match_in_line(line_text, pattern, case_sensitive, whole_word)
                        else {
                            continue;
                        };

                        let line = chunk.start_line + offset;
                        let column = line_text[..col_byte].chars().count() + 1;
                        matched_files.insert(chunk.file_path.clone());
                        matches.push(TextSearchMatch {
                            file: chunk.file_path.clone(),
                            line,
                            column,
                            text: line_text.to_string(),
                        });
                    }
                }
            }

            if truncated {
                let Some(mode) = next_state else {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Internal error: missing cursor state",
                    )]));
                };
                let token = TextSearchCursorV1 {
                    v: CURSOR_VERSION,
                    tool: "text_search".to_string(),
                    root: root_display.clone(),
                    pattern: pattern.to_string(),
                    file_pattern: normalized_file_pattern.clone(),
                    case_sensitive,
                    whole_word,
                    mode,
                };
                next_cursor = match encode_cursor(&token) {
                    Ok(value) => Some(value),
                    Err(err) => {
                        return Ok(CallToolResult::error(vec![Content::text(format!(
                            "Error: {err:#}"
                        ))]));
                    }
                };
            }
        } else {
            source = "filesystem".to_string();

            let scanner = FileScanner::new(&root);
            let (start_file_index, start_line_offset) = match cursor_mode.as_ref() {
                None => (0usize, 0usize),
                Some(TextSearchCursorModeV1::Filesystem {
                    file_index,
                    line_offset,
                }) => (*file_index, *line_offset),
                Some(TextSearchCursorModeV1::Corpus { .. }) => {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Invalid cursor: wrong mode",
                    )]));
                }
            };

            let mut candidates: Vec<(String, PathBuf)> = scanner
                .scan()
                .into_iter()
                .filter_map(|file| normalize_relative_path(&root, &file).map(|rel| (rel, file)))
                .filter(|(rel, _)| Self::matches_file_pattern(rel, file_pattern))
                .collect();
            candidates.sort_by(|a, b| a.0.cmp(&b.0));

            if start_file_index > candidates.len() {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: out of range",
                )]));
            }

            let mut next_state: Option<TextSearchCursorModeV1> = None;

            'outer_fs: for (file_index, (rel_path, abs_path)) in
                candidates.iter().enumerate().skip(start_file_index)
            {
                if matches.len() >= max_results {
                    truncated = true;
                    next_state = Some(TextSearchCursorModeV1::Filesystem {
                        file_index,
                        line_offset: 0,
                    });
                    break 'outer_fs;
                }

                scanned_files += 1;

                let meta = match std::fs::metadata(abs_path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.len() > MAX_FILE_BYTES {
                    skipped_large_files += 1;
                    continue;
                }

                let Ok(content) = std::fs::read_to_string(abs_path) else {
                    continue;
                };

                let first_file = file_index == start_file_index;
                let line_start = if first_file { start_line_offset } else { 0 };

                for (offset, line_text) in content.lines().enumerate().skip(line_start) {
                    if matches.len() >= max_results {
                        truncated = true;
                        next_state = Some(TextSearchCursorModeV1::Filesystem {
                            file_index,
                            line_offset: offset,
                        });
                        break 'outer_fs;
                    }

                    let Some(col_byte) =
                        Self::match_in_line(line_text, pattern, case_sensitive, whole_word)
                    else {
                        continue;
                    };
                    let column = line_text[..col_byte].chars().count() + 1;
                    matched_files.insert(rel_path.clone());
                    matches.push(TextSearchMatch {
                        file: rel_path.clone(),
                        line: offset + 1,
                        column,
                        text: line_text.to_string(),
                    });
                }
            }

            if truncated {
                let Some(mode) = next_state else {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Internal error: missing cursor state",
                    )]));
                };
                let token = TextSearchCursorV1 {
                    v: CURSOR_VERSION,
                    tool: "text_search".to_string(),
                    root: root_display.clone(),
                    pattern: pattern.to_string(),
                    file_pattern: normalized_file_pattern.clone(),
                    case_sensitive,
                    whole_word,
                    mode,
                };
                next_cursor = match encode_cursor(&token) {
                    Ok(value) => Some(value),
                    Err(err) => {
                        return Ok(CallToolResult::error(vec![Content::text(format!(
                            "Error: {err:#}"
                        ))]));
                    }
                };
            }
        }

        let result = TextSearchResult {
            pattern: pattern.to_string(),
            source,
            scanned_files,
            matched_files: matched_files.len(),
            skipped_large_files,
            returned: matches.len(),
            truncated,
            next_cursor,
            matches,
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Read a bounded slice of a file within the project root (safe file access for agents).
    #[tool(
        description = "Read a bounded slice of a file (by line) within the project root. Safe replacement for ad-hoc `cat/sed` reads; enforces max_lines/max_chars and prevents path traversal."
    )]
    pub async fn file_slice(
        &self,
        Parameters(request): Parameters<FileSliceRequest>,
    ) -> Result<CallToolResult, McpError> {
        const DEFAULT_MAX_LINES: usize = 200;
        const MAX_MAX_LINES: usize = 5_000;
        const DEFAULT_MAX_CHARS: usize = 20_000;
        const MAX_MAX_CHARS: usize = 500_000;

        let root_path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let root = match root_path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid path: {e}"
                ))]));
            }
        };
        self.touch_daemon_best_effort(&root);

        let file_str = request.file.trim();
        if file_str.is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "File must not be empty",
            )]));
        }

        let candidate = {
            let input_path = Path::new(file_str);
            if input_path.is_absolute() {
                PathBuf::from(input_path)
            } else {
                root.join(input_path)
            }
        };

        let canonical_file = match candidate.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid file '{file_str}': {e}"
                ))]));
            }
        };

        if !canonical_file.starts_with(&root) {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "File '{file_str}' is outside project root"
            ))]));
        }

        let display_file = normalize_relative_path(&root, &canonical_file).unwrap_or_else(|| {
            canonical_file
                .to_string_lossy()
                .into_owned()
                .replace('\\', "/")
        });

        let meta = match std::fs::metadata(&canonical_file) {
            Ok(m) => m,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Failed to stat '{display_file}': {e}"
                ))]));
            }
        };
        let file_size_bytes = meta.len();
        let file_mtime_ms = meta.modified().map(unix_ms).unwrap_or(0);

        let start_line = request.start_line.unwrap_or(1).max(1);
        let max_lines = request
            .max_lines
            .unwrap_or(DEFAULT_MAX_LINES)
            .clamp(1, MAX_MAX_LINES);
        let max_chars = request
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(1, MAX_MAX_CHARS);

        let file = match std::fs::File::open(&canonical_file) {
            Ok(f) => f,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Failed to open '{display_file}': {e}"
                ))]));
            }
        };
        let reader = BufReader::new(file);

        let mut content = String::new();
        let mut used_chars = 0usize;
        let mut returned_lines = 0usize;
        let mut end_line = 0usize;
        let mut truncated = false;
        let mut truncation: Option<FileSliceTruncation> = None;

        for (idx, line) in reader.lines().enumerate() {
            let line_no = idx + 1;
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Failed to read '{display_file}': {e}"
                    ))]));
                }
            };

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

        let result = FileSliceResult {
            file: display_file,
            start_line,
            end_line,
            returned_lines,
            used_chars,
            max_lines,
            max_chars,
            truncated,
            truncation,
            file_size_bytes,
            file_mtime_ms,
            content_sha256,
            content,
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// List project files within the project root (safe file enumeration for agents).
    #[tool(
        description = "List project file paths (relative to project root). Safe replacement for `ls/find/rg --files`; supports glob/substring filtering and bounded output."
    )]
    pub async fn list_files(
        &self,
        Parameters(request): Parameters<ListFilesRequest>,
    ) -> Result<CallToolResult, McpError> {
        const DEFAULT_LIMIT: usize = 200;
        const MAX_LIMIT: usize = 50_000;
        const DEFAULT_MAX_CHARS: usize = 20_000;
        const MAX_MAX_CHARS: usize = 500_000;

        let root_path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let root = match root_path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid path: {e}"
                ))]));
            }
        };
        self.touch_daemon_best_effort(&root);
        let root_display = root.to_string_lossy().to_string();

        let limit = request.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let max_chars = request
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(1, MAX_MAX_CHARS);

        let normalized_file_pattern = request
            .file_pattern
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let mut cursor_last_file: Option<String> = None;
        if let Some(cursor) = request
            .cursor
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let decoded: ListFilesCursorV1 = match decode_cursor(cursor) {
                Ok(v) => v,
                Err(err) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid cursor: {err}"
                    ))]));
                }
            };
            if decoded.v != CURSOR_VERSION || decoded.tool != "list_files" {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: wrong tool",
                )]));
            }
            if decoded.root != root_display {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: different root",
                )]));
            }
            if decoded.file_pattern != normalized_file_pattern {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: different file_pattern",
                )]));
            }
            cursor_last_file = Some(decoded.last_file);
        }
        let result = match compute_list_files_result(
            &root,
            &root_display,
            request.file_pattern.as_deref(),
            limit,
            max_chars,
            cursor_last_file.as_deref(),
        )
        .await
        {
            Ok(result) => result,
            Err(err) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {err:#}"
                ))]));
            }
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Regex search with merged context hunks (grep-like).
    #[tool(
        description = "Search project files with a regex and return merged context hunks (N lines before/after). Designed to replace `rg -C/-A/-B` plus multiple file_slice calls with a single bounded response."
    )]
    pub async fn grep_context(
        &self,
        Parameters(mut request): Parameters<GrepContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        const DEFAULT_MAX_CHARS: usize = 20_000;
        const MAX_MAX_CHARS: usize = 500_000;
        const DEFAULT_MAX_MATCHES: usize = 2_000;
        const MAX_MAX_MATCHES: usize = 50_000;
        const DEFAULT_MAX_HUNKS: usize = 200;
        const MAX_MAX_HUNKS: usize = 50_000;
        const DEFAULT_CONTEXT: usize = 20;
        const MAX_CONTEXT: usize = 5_000;

        let root_path = PathBuf::from(request.path.as_deref().unwrap_or("."));
        let root = match root_path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid path: {e}"
                ))]));
            }
        };
        self.touch_daemon_best_effort(&root);
        let root_display = root.to_string_lossy().to_string();

        let pattern = request.pattern.trim().to_string();
        if pattern.is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "Pattern must not be empty",
            )]));
        }
        request.pattern = pattern.clone();

        let case_sensitive = request.case_sensitive.unwrap_or(true);
        let regex = match RegexBuilder::new(&pattern)
            .case_insensitive(!case_sensitive)
            .build()
        {
            Ok(re) => re,
            Err(err) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid regex: {err}"
                ))]));
            }
        };

        let before = request
            .before
            .or(request.context)
            .unwrap_or(DEFAULT_CONTEXT)
            .clamp(0, MAX_CONTEXT);
        let after = request
            .after
            .or(request.context)
            .unwrap_or(DEFAULT_CONTEXT)
            .clamp(0, MAX_CONTEXT);

        let max_matches = request
            .max_matches
            .unwrap_or(DEFAULT_MAX_MATCHES)
            .clamp(1, MAX_MAX_MATCHES);
        let max_hunks = request
            .max_hunks
            .unwrap_or(DEFAULT_MAX_HUNKS)
            .clamp(1, MAX_MAX_HUNKS);
        let max_chars = request
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(1, MAX_MAX_CHARS);

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

        let mut resume_file: Option<String> = None;
        let mut resume_line = 1usize;
        if let Some(cursor) = request
            .cursor
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let decoded: GrepContextCursorV1 = match decode_cursor(cursor) {
                Ok(v) => v,
                Err(err) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid cursor: {err}"
                    ))]));
                }
            };
            if decoded.v != CURSOR_VERSION || decoded.tool != "grep_context" {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: wrong tool",
                )]));
            }
            if decoded.root != root_display {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: different root",
                )]));
            }
            if decoded.pattern != request.pattern {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: different pattern",
                )]));
            }
            if decoded.file != normalized_file {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: different file",
                )]));
            }
            if decoded.file_pattern != normalized_file_pattern {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: different file_pattern",
                )]));
            }
            if decoded.case_sensitive != case_sensitive
                || decoded.before != before
                || decoded.after != after
            {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Invalid cursor: different search options",
                )]));
            }
            resume_file = Some(decoded.resume_file);
            resume_line = decoded.resume_line.max(1);
        }

        let result = match compute_grep_context_result(
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
                resume_file: resume_file.as_deref(),
                resume_line,
            },
        )
        .await
        {
            Ok(result) => result,
            Err(err) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {err:#}"
                ))]));
            }
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Execute multiple Context Finder tools in a single call (agent-friendly batch).
    #[tool(
        description = "Execute multiple Context Finder tools in one call. Returns a single bounded JSON result with per-item status (partial success) and a global max_chars budget."
    )]
    pub async fn batch(
        &self,
        Parameters(request): Parameters<BatchRequest>,
    ) -> Result<CallToolResult, McpError> {
        const DEFAULT_MAX_CHARS: usize = 20_000;
        const MAX_MAX_CHARS: usize = 500_000;
        const DEFAULT_VERSION: u32 = 1;
        const LATEST_VERSION: u32 = 2;

        if request.items.is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "Batch items must not be empty",
            )]));
        }

        let max_chars = request
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(1, MAX_MAX_CHARS);

        let version = request.version.unwrap_or(DEFAULT_VERSION);
        if !(DEFAULT_VERSION..=LATEST_VERSION).contains(&version) {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Unsupported batch version {version} (supported: {DEFAULT_VERSION}..={LATEST_VERSION})"
            ))]));
        }

        let mut output = BatchResult {
            version,
            items: Vec::new(),
            budget: BatchBudget {
                max_chars,
                used_chars: 0,
                truncated: false,
            },
        };

        let mut inferred_path: Option<String> = request.path.as_ref().map(|p| p.trim().to_string());
        let mut ref_context = if version >= 2 {
            Some(serde_json::json!({
                "path": inferred_path.clone(),
                "items": serde_json::Value::Object(serde_json::Map::new()),
            }))
        } else {
            None
        };

        if version >= 2 {
            let mut seen = HashSet::new();
            for item in &request.items {
                let id = item.id.trim();
                if id.is_empty() {
                    continue;
                }
                if !seen.insert(id.to_string()) {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Batch v2 requires unique item ids (duplicate: '{id}')"
                    ))]));
                }
            }
        }

        for item in request.items {
            let id = item.id.trim().to_string();
            if id.is_empty() {
                let rejected = BatchItemResult {
                    id: item.id,
                    tool: item.tool,
                    status: BatchItemStatus::Error,
                    message: Some("Batch item id must not be empty".to_string()),
                    data: serde_json::Value::Null,
                };
                if !push_item_or_truncate(&mut output, rejected) {
                    break;
                }
                if request.stop_on_error {
                    break;
                }
                continue;
            }

            let resolved_input = if let Some(ctx) = ref_context.as_ref() {
                match resolve_batch_refs(item.input, ctx) {
                    Ok(value) => value,
                    Err(err) => {
                        let rejected = BatchItemResult {
                            id,
                            tool: item.tool,
                            status: BatchItemStatus::Error,
                            message: Some(format!("Ref resolution error: {err}")),
                            data: serde_json::Value::Null,
                        };
                        if !push_item_or_truncate(&mut output, rejected) {
                            break;
                        }
                        if request.stop_on_error {
                            break;
                        }
                        continue;
                    }
                }
            } else {
                item.input
            };

            let item_path = extract_path_from_input(&resolved_input);
            if inferred_path.is_none() {
                inferred_path = item_path.clone();
            } else if let (Some(batch_path), Some(item_path)) = (&inferred_path, item_path) {
                if batch_path != &item_path {
                    let rejected = BatchItemResult {
                        id,
                        tool: item.tool,
                        status: BatchItemStatus::Error,
                        message: Some(format!(
                            "Batch path mismatch: batch uses '{batch_path}', item uses '{item_path}'"
                        )),
                        data: serde_json::Value::Null,
                    };
                    if !push_item_or_truncate(&mut output, rejected) {
                        break;
                    }
                    if request.stop_on_error {
                        break;
                    }
                    continue;
                }
            }

            if let Some(ctx) = ref_context.as_mut() {
                ctx["path"] = inferred_path
                    .as_ref()
                    .map(|v| serde_json::Value::String(v.clone()))
                    .unwrap_or(serde_json::Value::Null);
            }

            let remaining_chars = output
                .budget
                .max_chars
                .saturating_sub(output.budget.used_chars);
            let input = prepare_item_input(
                resolved_input,
                inferred_path.as_deref(),
                item.tool,
                remaining_chars,
            );

            let tool_result: Result<CallToolResult, McpError> = match item.tool {
                BatchToolName::Map => match serde_json::from_value::<MapRequest>(input) {
                    Ok(req) => self.map(Parameters(req)).await,
                    Err(err) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid input for map: {err}"
                    ))])),
                },
                BatchToolName::FileSlice => match serde_json::from_value::<FileSliceRequest>(input)
                {
                    Ok(req) => self.file_slice(Parameters(req)).await,
                    Err(err) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid input for file_slice: {err}"
                    ))])),
                },
                BatchToolName::ListFiles => match serde_json::from_value::<ListFilesRequest>(input)
                {
                    Ok(req) => self.list_files(Parameters(req)).await,
                    Err(err) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid input for list_files: {err}"
                    ))])),
                },
                BatchToolName::TextSearch => {
                    match serde_json::from_value::<TextSearchRequest>(input) {
                        Ok(req) => self.text_search(Parameters(req)).await,
                        Err(err) => Ok(CallToolResult::error(vec![Content::text(format!(
                            "Invalid input for text_search: {err}"
                        ))])),
                    }
                }
                BatchToolName::GrepContext => {
                    match serde_json::from_value::<GrepContextRequest>(input) {
                        Ok(req) => self.grep_context(Parameters(req)).await,
                        Err(err) => Ok(CallToolResult::error(vec![Content::text(format!(
                            "Invalid input for grep_context: {err}"
                        ))])),
                    }
                }
                BatchToolName::Doctor => match serde_json::from_value::<DoctorRequest>(input) {
                    Ok(req) => self.doctor(Parameters(req)).await,
                    Err(err) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid input for doctor: {err}"
                    ))])),
                },
                BatchToolName::Search => match serde_json::from_value::<SearchRequest>(input) {
                    Ok(req) => self.search(Parameters(req)).await,
                    Err(err) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid input for search: {err}"
                    ))])),
                },
                BatchToolName::Context => match serde_json::from_value::<ContextRequest>(input) {
                    Ok(req) => self.context(Parameters(req)).await,
                    Err(err) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid input for context: {err}"
                    ))])),
                },
                BatchToolName::ContextPack => {
                    match serde_json::from_value::<ContextPackRequest>(input) {
                        Ok(req) => self.context_pack(Parameters(req)).await,
                        Err(err) => Ok(CallToolResult::error(vec![Content::text(format!(
                            "Invalid input for context_pack: {err}"
                        ))])),
                    }
                }
                BatchToolName::Index => match serde_json::from_value::<IndexRequest>(input) {
                    Ok(req) => self.index(Parameters(req)).await,
                    Err(err) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid input for index: {err}"
                    ))])),
                },
                BatchToolName::Impact => match serde_json::from_value::<ImpactRequest>(input) {
                    Ok(req) => self.impact(Parameters(req)).await,
                    Err(err) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid input for impact: {err}"
                    ))])),
                },
                BatchToolName::Trace => match serde_json::from_value::<TraceRequest>(input) {
                    Ok(req) => self.trace(Parameters(req)).await,
                    Err(err) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid input for trace: {err}"
                    ))])),
                },
                BatchToolName::Explain => match serde_json::from_value::<ExplainRequest>(input) {
                    Ok(req) => self.explain(Parameters(req)).await,
                    Err(err) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid input for explain: {err}"
                    ))])),
                },
                BatchToolName::Overview => match serde_json::from_value::<OverviewRequest>(input) {
                    Ok(req) => self.overview(Parameters(req)).await,
                    Err(err) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid input for overview: {err}"
                    ))])),
                },
            };

            let item_outcome = match tool_result {
                Ok(result) => match parse_tool_result_as_json(&result, item.tool) {
                    Ok(data) => BatchItemResult {
                        id,
                        tool: item.tool,
                        status: BatchItemStatus::Ok,
                        message: None,
                        data,
                    },
                    Err(message) => BatchItemResult {
                        id,
                        tool: item.tool,
                        status: BatchItemStatus::Error,
                        message: Some(message),
                        data: serde_json::Value::Null,
                    },
                },
                Err(err) => BatchItemResult {
                    id,
                    tool: item.tool,
                    status: BatchItemStatus::Error,
                    message: Some(err.to_string()),
                    data: serde_json::Value::Null,
                },
            };

            if !push_item_or_truncate(&mut output, item_outcome) {
                break;
            }

            if let Some(ctx) = ref_context.as_mut() {
                let Some(items) = ctx
                    .get_mut("items")
                    .and_then(serde_json::Value::as_object_mut)
                else {
                    break;
                };
                if let Some(stored) = output.items.last() {
                    items.insert(
                        stored.id.clone(),
                        serde_json::json!({
                            "tool": stored.tool,
                            "status": stored.status,
                            "message": stored.message,
                            "data": stored.data,
                        }),
                    );
                }
            }

            if request.stop_on_error
                && output
                    .items
                    .last()
                    .is_some_and(|v| v.status == BatchItemStatus::Error)
            {
                break;
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&output).unwrap_or_default(),
        )]))
    }

    /// Diagnose model/GPU/index configuration
    #[tool(
        description = "Show diagnostics for model directory, CUDA/ORT runtime, and per-project index/corpus status. Use this when something fails (e.g., GPU provider missing)."
    )]
    pub async fn doctor(
        &self,
        Parameters(request): Parameters<DoctorRequest>,
    ) -> Result<CallToolResult, McpError> {
        let model_dir = context_vector_store::model_dir();
        let manifest_path = model_dir.join("manifest.json");

        let (model_manifest_exists, models) = match load_model_statuses(&model_dir).await {
            Ok(result) => result,
            Err(err) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Failed to load model manifest {}: {err:#}",
                    manifest_path.display()
                ))]));
            }
        };

        let gpu = runtime_env::diagnose_gpu_env();
        let cuda_disabled = runtime_env::is_cuda_disabled();
        let allow_cpu_fallback = std::env::var("CONTEXT_FINDER_ALLOW_CPU")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let mut issues: Vec<String> = Vec::new();
        let mut hints: Vec<String> = Vec::new();

        if !cuda_disabled && (!gpu.provider_present || !gpu.cublas_present) {
            issues
                .push("CUDA libraries are not fully configured (provider/cublas missing).".into());
            hints.push("Run `bash scripts/setup_cuda_deps.sh` in the Context Finder repo, or set ORT_LIB_LOCATION/LD_LIBRARY_PATH to directories containing libonnxruntime_providers_cuda.so and libcublasLt.so.*. If you want CPU fallback, set CONTEXT_FINDER_ALLOW_CPU=1.".into());
        }

        if !model_manifest_exists {
            issues.push(format!(
                "Model manifest not found at {}",
                manifest_path.display()
            ));
            hints.push("Run `context-finder install-models` (or set CONTEXT_FINDER_MODEL_DIR to a directory containing models/manifest.json).".into());
        } else if models.iter().any(|m| !m.installed) {
            hints.push("Some models are missing assets. Run `context-finder install-models` to download them into the model directory.".into());
        }

        let project = match request.path {
            None => None,
            Some(raw) => {
                let root = match PathBuf::from(raw).canonicalize() {
                    Ok(p) => Some(p),
                    Err(err) => {
                        issues.push(format!("Invalid project path: {err}"));
                        None
                    }
                };

                if let Some(root) = root {
                    let corpus_path = corpus_path_for_project_root(&root);
                    let has_corpus = corpus_path.exists();

                    let indexes_dir = root.join(".context-finder").join("indexes");
                    let mut indexed_models: Vec<String> = Vec::new();
                    if let Ok(entries) = std::fs::read_dir(&indexes_dir) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if !path.is_dir() {
                                continue;
                            }
                            let index_path = path.join("index.json");
                            if index_path.exists() {
                                indexed_models
                                    .push(entry.file_name().to_string_lossy().into_owned());
                            }
                        }
                    }
                    indexed_models.sort();

                    if indexed_models.is_empty() {
                        hints.push(
                            "No semantic indexes found for this project. Run the `index` tool first."
                                .into(),
                        );
                    }

                    let mut drift: Vec<DoctorIndexDrift> = Vec::new();
                    if has_corpus && !indexed_models.is_empty() {
                        match load_corpus_chunk_ids(&corpus_path).await {
                            Ok(corpus_ids) => {
                                let corpus_chunks = corpus_ids.len();
                                let mut drifted_models = Vec::new();

                                for model_id in &indexed_models {
                                    let index_path = indexes_dir.join(model_id).join("index.json");
                                    let index_ids = match load_index_chunk_ids(&index_path).await {
                                        Ok(ids) => ids,
                                        Err(err) => {
                                            issues.push(format!(
                                                "Failed to read index for model '{model_id}': {err:#}"
                                            ));
                                            continue;
                                        }
                                    };

                                    let missing_chunks = corpus_ids.difference(&index_ids).count();
                                    let extra_chunks = index_ids.difference(&corpus_ids).count();

                                    if missing_chunks > 0 || extra_chunks > 0 {
                                        drifted_models.push(model_id.clone());
                                    }

                                    let missing_file_samples =
                                        sample_file_paths(corpus_ids.difference(&index_ids), 8);
                                    let extra_file_samples =
                                        sample_file_paths(index_ids.difference(&corpus_ids), 8);

                                    drift.push(DoctorIndexDrift {
                                        model: model_id.clone(),
                                        index_path: index_path.to_string_lossy().into_owned(),
                                        index_chunks: index_ids.len(),
                                        corpus_chunks,
                                        missing_chunks,
                                        extra_chunks,
                                        missing_file_samples,
                                        extra_file_samples,
                                    });
                                }

                                if !drifted_models.is_empty() {
                                    issues.push(format!(
                                        "Index drift detected vs corpus for models: {}",
                                        drifted_models.join(", ")
                                    ));
                                    hints.push("Run `context-finder index --force --experts` (or the MCP `index` tool) to rebuild semantic indexes to match the current corpus. If you recently changed profiles/models, consider reindexing all models in your roster.".into());
                                }
                            }
                            Err(err) => {
                                issues.push(format!(
                                    "Failed to load corpus {}: {err:#}",
                                    corpus_path.display()
                                ));
                            }
                        }
                    } else if !has_corpus && !indexed_models.is_empty() {
                        hints.push("Corpus not found for this project; drift detection is unavailable. Run `context-finder index` once to generate corpus + indexes.".into());
                    }

                    Some(DoctorProjectResult {
                        root: root.to_string_lossy().into_owned(),
                        corpus_path: corpus_path.to_string_lossy().into_owned(),
                        has_corpus,
                        indexed_models,
                        drift,
                    })
                } else {
                    None
                }
            }
        };

        let result = DoctorResult {
            env: DoctorEnvResult {
                profile: self.profile.name().to_string(),
                model_dir: model_dir.to_string_lossy().into_owned(),
                model_manifest_exists,
                models,
                gpu,
                cuda_disabled,
                allow_cpu_fallback,
            },
            project,
            issues,
            hints,
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Semantic code search
    #[tool(
        description = "Search for code using natural language. Returns relevant code snippets with file locations and symbols."
    )]
    pub async fn search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let limit = request.limit.unwrap_or(10).clamp(1, 50);

        if request.query.trim().is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "Error: Query cannot be empty",
            )]));
        }

        let root = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid path: {e}"
                ))]));
            }
        };

        let mut engine = match self.lock_engine(&root).await {
            Ok(engine) => engine,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {e}"
                ))]));
            }
        };

        let results = match engine
            .engine_mut()
            .context_search
            .hybrid_mut()
            .search(&request.query, limit)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Search error: {e}"
                ))]));
            }
        };

        let formatted: Vec<SearchResult> = results
            .into_iter()
            .map(|r| SearchResult {
                file: r.chunk.file_path.clone(),
                start_line: r.chunk.start_line,
                end_line: r.chunk.end_line,
                symbol: r.chunk.metadata.symbol_name.clone(),
                symbol_type: r
                    .chunk
                    .metadata
                    .chunk_type
                    .map(|ct| ct.as_str().to_string()),
                score: r.score,
                content: r.chunk.content.clone(),
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&formatted).unwrap_or_default(),
        )]))
    }

    /// Search with graph context
    #[tool(
        description = "Search for code with automatic graph-based context. Returns code plus related functions/types through call graphs and dependencies. Best for understanding how code connects."
    )]
    pub async fn context(
        &self,
        Parameters(request): Parameters<ContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let limit = request.limit.unwrap_or(5).clamp(1, 20);
        let strategy = match request.strategy.as_deref() {
            Some("direct") => context_graph::AssemblyStrategy::Direct,
            Some("deep") => context_graph::AssemblyStrategy::Deep,
            _ => context_graph::AssemblyStrategy::Extended,
        };

        if request.query.trim().is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "Error: Query cannot be empty",
            )]));
        }

        let root = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid path: {e}"
                ))]));
            }
        };

        let mut engine = match self.lock_engine(&root).await {
            Ok(engine) => engine,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {e}"
                ))]));
            }
        };

        let language = match request.language.as_deref() {
            Some(lang) => Self::parse_language(Some(lang)),
            None => Self::detect_language(engine.engine_mut().context_search.hybrid().chunks()),
        };

        if let Err(e) = engine.engine_mut().ensure_graph(language).await {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Graph build error: {e}"
            ))]));
        }

        let enriched = match engine
            .engine_mut()
            .context_search
            .search_with_context(&request.query, limit, strategy)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Search error: {e}"
                ))]));
            }
        };

        let mut related_count = 0;
        let results: Vec<ContextHit> = enriched
            .into_iter()
            .map(|er| {
                let related: Vec<RelatedCode> = er
                    .related
                    .iter()
                    .take(5)
                    .map(|rc| {
                        related_count += 1;
                        RelatedCode {
                            file: rc.chunk.file_path.clone(),
                            lines: format!("{}-{}", rc.chunk.start_line, rc.chunk.end_line),
                            symbol: rc.chunk.metadata.symbol_name.clone(),
                            relationship: rc.relationship_path.join(" -> "),
                        }
                    })
                    .collect();

                ContextHit {
                    file: er.primary.chunk.file_path.clone(),
                    start_line: er.primary.chunk.start_line,
                    end_line: er.primary.chunk.end_line,
                    symbol: er.primary.chunk.metadata.symbol_name.clone(),
                    score: er.primary.score,
                    content: er.primary.chunk.content.clone(),
                    related,
                }
            })
            .collect();

        let result = ContextResult {
            results,
            related_count,
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Build a bounded context pack for agents (single-call context).
    #[tool(
        description = "Build a bounded `context_pack` JSON for a query: primary hits + graph-related halo, under a strict character budget. Intended as the single-call payload for AI agents."
    )]
    pub async fn context_pack(
        &self,
        Parameters(request): Parameters<ContextPackRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let limit = request.limit.unwrap_or(10).clamp(1, 50);
        let max_chars = request.max_chars.unwrap_or(20_000).max(1_000);
        let max_related_per_primary = request.max_related_per_primary.unwrap_or(3).clamp(0, 12);
        let trace = request.trace.unwrap_or(false);
        let auto_index = request.auto_index.unwrap_or(false);
        let strategy = match request.strategy.as_deref() {
            Some("direct") => context_graph::AssemblyStrategy::Direct,
            Some("deep") => context_graph::AssemblyStrategy::Deep,
            _ => context_graph::AssemblyStrategy::Extended,
        };

        if request.query.trim().is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "Error: Query cannot be empty",
            )]));
        }

        let root = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid path: {e}"
                ))]));
            }
        };

        let mut engine = match self.lock_engine(&root).await {
            Ok(engine) => engine,
            Err(e) => {
                if !auto_index {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Error: {e}"
                    ))]));
                }

                if let Err(index_err) = self.auto_index_project(&root).await {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Auto-index error: {index_err}"
                    ))]));
                }

                match self.lock_engine(&root).await {
                    Ok(engine) => engine,
                    Err(e) => {
                        return Ok(CallToolResult::error(vec![Content::text(format!(
                            "Error after auto-index: {e}"
                        ))]));
                    }
                }
            }
        };

        let language = match request.language.as_deref() {
            Some(lang) => Self::parse_language(Some(lang)),
            None => Self::detect_language(engine.engine_mut().context_search.hybrid().chunks()),
        };

        if let Err(e) = engine.engine_mut().ensure_graph(language).await {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Graph build error: {e}"
            ))]));
        }

        let available_models = engine.engine_mut().available_models.clone();
        let source_index_mtime_ms = unix_ms(engine.engine_mut().canonical_index_mtime);

        let mut enriched = match engine
            .engine_mut()
            .context_search
            .search_with_context(&request.query, limit, strategy)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Search error: {e}"
                ))]));
            }
        };

        // Optional graph_nodes channel for conceptual queries (graph-as-text embeddings).
        let query_type = QueryClassifier::classify(&request.query);
        let graph_nodes_cfg = self.profile.graph_nodes();
        if graph_nodes_cfg.enabled
            && !matches!(strategy, context_graph::AssemblyStrategy::Direct)
            && matches!(query_type, QueryType::Conceptual)
        {
            let engine_ref = engine.engine_mut();
            if let Some(assembler) = engine_ref.context_search.assembler() {
                let chunks = engine_ref.context_search.hybrid().chunks();
                let chunk_lookup = &engine_ref.chunk_lookup;

                let graph_nodes_path = graph_nodes_store_path(&root);
                let language_key = graph_language_key(language).to_string();

                let template_hash = self.profile.embedding().graph_node_template_hash();
                let graph_nodes_store =
                    if let Ok(store) = GraphNodeStore::load(&graph_nodes_path).await {
                        let meta = store.meta();
                        if meta.source_index_mtime_ms == source_index_mtime_ms
                            && meta.graph_language == language_key
                            && meta.graph_doc_version == GRAPH_DOC_VERSION
                            && meta.template_hash == template_hash
                        {
                            Some(store)
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                let graph_nodes_store = match graph_nodes_store {
                    Some(store) => store,
                    None => {
                        let docs = build_graph_docs(
                            assembler,
                            GraphDocConfig {
                                max_neighbors_per_relation: graph_nodes_cfg
                                    .max_neighbors_per_relation,
                            },
                        );
                        let docs: Vec<GraphNodeDoc> = docs
                            .into_iter()
                            .map(|doc| {
                                let text = self
                                    .profile
                                    .embedding()
                                    .render_graph_node_doc(&doc.doc)
                                    .unwrap_or(doc.doc);
                                GraphNodeDoc {
                                    node_id: doc.node_id,
                                    chunk_id: doc.chunk_id,
                                    text,
                                    doc_hash: doc.doc_hash,
                                }
                            })
                            .collect();

                        let meta = match GraphNodeStoreMeta::for_current_model(
                            source_index_mtime_ms,
                            language_key,
                            GRAPH_DOC_VERSION,
                            template_hash,
                        ) {
                            Ok(m) => m,
                            Err(err) => {
                                return Ok(CallToolResult::error(vec![Content::text(format!(
                                    "graph_nodes meta error: {err}"
                                ))]));
                            }
                        };

                        match GraphNodeStore::build_or_update(&graph_nodes_path, meta, docs).await {
                            Ok(store) => store,
                            Err(err) => {
                                return Ok(CallToolResult::error(vec![Content::text(format!(
                                    "graph_nodes build error: {err}"
                                ))]));
                            }
                        }
                    }
                };

                let embedding_query = self
                    .profile
                    .embedding()
                    .render_query(context_vector_store::QueryKind::Conceptual, &request.query)
                    .unwrap_or_else(|_| request.query.clone());
                let hits = graph_nodes_store
                    .search_with_embedding_text(&embedding_query, graph_nodes_cfg.top_k)
                    .await
                    .unwrap_or_default();

                if !hits.is_empty() {
                    const RRF_K: f32 = 60.0;
                    let mut fused: HashMap<String, f32> = HashMap::new();

                    for (rank, er) in enriched.iter().enumerate() {
                        #[allow(clippy::cast_precision_loss)]
                        let contrib = 1.0 / (RRF_K + (rank as f32) + 1.0);
                        fused
                            .entry(er.primary.id.clone())
                            .and_modify(|v| *v += contrib)
                            .or_insert(contrib);
                    }

                    for (rank, hit) in hits.iter().enumerate() {
                        #[allow(clippy::cast_precision_loss)]
                        let contrib = graph_nodes_cfg.weight / (RRF_K + (rank as f32) + 1.0);
                        fused
                            .entry(hit.chunk_id.clone())
                            .and_modify(|v| *v += contrib)
                            .or_insert(contrib);
                    }

                    let mut have_primary: HashSet<String> =
                        enriched.iter().map(|er| er.primary.id.clone()).collect();

                    for hit in hits {
                        if have_primary.contains(&hit.chunk_id) {
                            continue;
                        }
                        let Some(&chunk_idx) = chunk_lookup.get(&hit.chunk_id) else {
                            continue;
                        };
                        let Some(chunk) = chunks.get(chunk_idx).cloned() else {
                            continue;
                        };
                        if self.profile.is_rejected(&chunk.file_path) {
                            continue;
                        }

                        let mut related = Vec::new();
                        let mut total_lines = chunk.line_count();
                        if let Ok(assembled) = assembler.assemble_for_chunk(&hit.chunk_id, strategy)
                        {
                            total_lines = assembled.total_lines;
                            related = assembled
                                .related_chunks
                                .into_iter()
                                .map(|rc| context_search::RelatedContext {
                                    chunk: rc.chunk,
                                    relationship_path: rc
                                        .relationship
                                        .iter()
                                        .map(|r| format!("{r:?}"))
                                        .collect(),
                                    distance: rc.distance,
                                    relevance_score: rc.relevance_score,
                                })
                                .collect();
                        }

                        enriched.push(context_search::EnrichedResult {
                            primary: context_search::SearchResult {
                                chunk,
                                score: 0.0,
                                id: hit.chunk_id.clone(),
                            },
                            related,
                            total_lines,
                            strategy,
                        });
                        have_primary.insert(hit.chunk_id);
                    }

                    let mut min_score = f32::MAX;
                    let mut max_score = f32::MIN;
                    for er in &enriched {
                        if let Some(score) = fused.get(&er.primary.id) {
                            min_score = min_score.min(*score);
                            max_score = max_score.max(*score);
                        }
                    }
                    let range = (max_score - min_score).max(1e-9);

                    for er in &mut enriched {
                        if let Some(score) = fused.get(&er.primary.id) {
                            er.primary.score = if range <= 1e-9 {
                                1.0
                            } else {
                                (*score - min_score) / range
                            };
                        }
                    }

                    enriched.sort_by(|a, b| {
                        b.primary
                            .score
                            .total_cmp(&a.primary.score)
                            .then_with(|| a.primary.id.cmp(&b.primary.id))
                    });
                    enriched.truncate(limit);
                }
            }
        }

        let (items, budget) =
            pack_enriched_results(&self.profile, enriched, max_chars, max_related_per_primary);
        let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
        let output = ContextPackOutput {
            version: CONTEXT_PACK_VERSION,
            query: request.query,
            model_id,
            profile: self.profile.name().to_string(),
            items,
            budget,
        };

        let mut contents = Vec::new();
        contents.push(Content::text(
            serde_json::to_string_pretty(&output).unwrap_or_default(),
        ));

        if trace {
            let query_kind = match query_type {
                QueryType::Identifier => QueryKind::Identifier,
                QueryType::Path => QueryKind::Path,
                QueryType::Conceptual => QueryKind::Conceptual,
            };
            let desired_models: Vec<String> =
                self.profile.experts().semantic_models(query_kind).to_vec();

            let debug = serde_json::json!({
                "query_kind": format!("{query_kind:?}"),
                "strategy": format!("{strategy:?}"),
                "language": graph_language_key(language),
                "semantic_models": {
                    "available": available_models,
                    "desired": desired_models,
                },
                "graph_nodes": {
                    "enabled": graph_nodes_cfg.enabled,
                    "weight": graph_nodes_cfg.weight,
                    "top_k": graph_nodes_cfg.top_k,
                    "max_neighbors_per_relation": graph_nodes_cfg.max_neighbors_per_relation,
                }
            });
            contents.push(Content::text(
                serde_json::to_string_pretty(&debug).unwrap_or_default(),
            ));
        }

        Ok(CallToolResult::success(contents))
    }

    /// Index a project
    #[tool(
        description = "Index a project directory for semantic search. Required before using search/context tools on a new project."
    )]
    pub async fn index(
        &self,
        Parameters(request): Parameters<IndexRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let force = request.force.unwrap_or(false);
        let full = request.full.unwrap_or(false) || force;
        let experts = request.experts.unwrap_or(false);
        let extra_models = request.models.unwrap_or_default();

        let canonical = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid path: {e}"
                ))]));
            }
        };
        self.touch_daemon_best_effort(&canonical);

        let start = std::time::Instant::now();

        let primary_model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
        let templates = self.profile.embedding().clone();

        let mut models: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        seen.insert(primary_model_id.clone());
        models.push(primary_model_id.clone());

        if experts {
            let expert_cfg = self.profile.experts();
            for kind in [
                QueryKind::Identifier,
                QueryKind::Path,
                QueryKind::Conceptual,
            ] {
                for model_id in expert_cfg.semantic_models(kind) {
                    if seen.insert(model_id.clone()) {
                        models.push(model_id.clone());
                    }
                }
            }
        }

        for model_id in extra_models {
            if seen.insert(model_id.clone()) {
                models.push(model_id);
            }
        }

        let registry = match context_vector_store::ModelRegistry::from_env() {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Model registry error: {e}"
                ))]));
            }
        };
        for model_id in &models {
            if let Err(e) = registry.dimension(model_id) {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Unknown or unsupported model_id '{model_id}': {e}"
                ))]));
            }
        }

        let specs: Vec<context_indexer::ModelIndexSpec> = models
            .iter()
            .map(|model_id| {
                context_indexer::ModelIndexSpec::new(model_id.clone(), templates.clone())
            })
            .collect();

        let indexer = match context_indexer::MultiModelProjectIndexer::new(&canonical).await {
            Ok(i) => i,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Indexer init error: {e}"
                ))]));
            }
        };

        let stats = match indexer.index_models(&specs, full).await {
            Ok(s) => s,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Indexing error: {e}"
                ))]));
            }
        };

        let time_ms = start.elapsed().as_millis() as u64;
        let index_path = index_path_for_model(&canonical, &primary_model_id);

        let result = IndexResult {
            files: stats.files,
            chunks: stats.chunks,
            time_ms,
            index_path: index_path.to_string_lossy().to_string(),
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Find all usages of a symbol (impact analysis)
    #[tool(
        description = "Find all places where a symbol is used. Essential for refactoring - shows direct usages, transitive dependencies, and related tests."
    )]
    pub async fn impact(
        &self,
        Parameters(request): Parameters<ImpactRequest>,
    ) -> Result<CallToolResult, McpError> {
        const MAX_DIRECT: usize = 200;
        const MAX_TRANSITIVE: usize = 200;

        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let depth = request.depth.unwrap_or(2).clamp(1, 3);
        let root = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid path: {e}"
                ))]));
            }
        };

        let mut engine = match self.lock_engine(&root).await {
            Ok(engine) => engine,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {e}"
                ))]));
            }
        };

        let language = match request.language.as_deref() {
            Some(lang) => Self::parse_language(Some(lang)),
            None => Self::detect_language(engine.engine_mut().context_search.hybrid().chunks()),
        };

        let symbol = request.symbol;
        let graph_ready = engine.engine_mut().ensure_graph(language).await.is_ok();

        if !graph_ready {
            // Best-effort UX: even when graph build fails (unsupported language), return
            // bounded TextMatch hits instead of hard-failing.
            let chunks = engine.engine_mut().context_search.hybrid().chunks();
            let direct = Self::find_text_usages(chunks, &symbol, None, MAX_DIRECT);
            let mermaid = Self::generate_impact_mermaid(&symbol, &direct, &[]);
            let files_affected: HashSet<&str> = direct.iter().map(|u| u.file.as_str()).collect();

            let result = ImpactResult {
                symbol,
                definition: None,
                total_usages: direct.len(),
                files_affected: files_affected.len(),
                direct,
                transitive: Vec::new(),
                tests: Vec::new(),
                public_api: false,
                mermaid,
            };

            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            )]));
        }

        let engine_ref = engine.engine_mut();
        let chunks = engine_ref.context_search.hybrid().chunks();

        let Some(assembler) = engine_ref.context_search.assembler() else {
            // Same best-effort behavior if graph isn't available after a successful build.
            let direct = Self::find_text_usages(chunks, &symbol, None, MAX_DIRECT);
            let mermaid = Self::generate_impact_mermaid(&symbol, &direct, &[]);
            let files_affected: HashSet<&str> = direct.iter().map(|u| u.file.as_str()).collect();

            let result = ImpactResult {
                symbol,
                definition: None,
                total_usages: direct.len(),
                files_affected: files_affected.len(),
                direct,
                transitive: Vec::new(),
                tests: Vec::new(),
                public_api: false,
                mermaid,
            };

            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            )]));
        };
        let graph = assembler.graph();

        let node = graph.find_node(&symbol);

        if node.is_none() {
            // Best-effort UX: when the symbol is not present in the graph (missing/ambiguous),
            // return bounded TextMatch hits instead of hard-failing.
            let direct = Self::find_text_usages(chunks, &symbol, None, MAX_DIRECT);
            let mermaid = Self::generate_impact_mermaid(&symbol, &direct, &[]);
            let files_affected: HashSet<&str> = direct.iter().map(|u| u.file.as_str()).collect();

            let result = ImpactResult {
                symbol,
                definition: None,
                total_usages: direct.len(),
                files_affected: files_affected.len(),
                direct,
                transitive: Vec::new(),
                tests: Vec::new(),
                public_api: false,
                mermaid,
            };

            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            )]));
        }

        let node = node.expect("node checked above");

        // Get definition location
        let definition = graph.get_node(node).map(|nd| SymbolLocation {
            file: nd.symbol.file_path.clone(),
            line: nd.symbol.start_line,
        });

        // Get direct usages (filter unknown symbols and markdown files)
        let direct_usages = graph.get_all_usages(node);
        let mut seen_direct: HashSet<(String, usize)> = HashSet::new();
        let mut direct: Vec<UsageInfo> = direct_usages
            .iter()
            .filter_map(|(n, rel)| {
                graph.get_node(*n).and_then(|nd| {
                    // Skip unknown symbols and markdown files
                    if nd.symbol.name == "unknown" || nd.symbol.file_path.ends_with(".md") {
                        return None;
                    }
                    // Deduplicate by (file, line)
                    let key = (nd.symbol.file_path.clone(), nd.symbol.start_line);
                    if seen_direct.contains(&key) {
                        return None;
                    }
                    seen_direct.insert(key);
                    Some(UsageInfo {
                        file: nd.symbol.file_path.clone(),
                        line: nd.symbol.start_line,
                        symbol: nd.symbol.name.clone(),
                        relationship: format!("{:?}", rel),
                    })
                })
            })
            .collect();
        if direct.len() > MAX_DIRECT {
            direct.truncate(MAX_DIRECT);
        }

        // Get transitive usages if depth > 1
        let transitive_usages = if depth > 1 {
            graph.get_transitive_usages(node, depth)
        } else {
            vec![]
        };
        let mut seen_transitive: HashSet<(String, usize)> = HashSet::new();
        let mut transitive: Vec<UsageInfo> = transitive_usages
            .iter()
            .filter(|(_, d, _)| *d > 1)
            .filter_map(|(n, _, path)| {
                graph.get_node(*n).and_then(|nd| {
                    // Skip unknown symbols and markdown files
                    if nd.symbol.name == "unknown" || nd.symbol.file_path.ends_with(".md") {
                        return None;
                    }
                    // Deduplicate by (file, line)
                    let key = (nd.symbol.file_path.clone(), nd.symbol.start_line);
                    if seen_transitive.contains(&key) {
                        return None;
                    }
                    seen_transitive.insert(key);
                    Some(UsageInfo {
                        file: nd.symbol.file_path.clone(),
                        line: nd.symbol.start_line,
                        symbol: nd.symbol.name.clone(),
                        relationship: path
                            .iter()
                            .map(|r| format!("{:?}", r))
                            .collect::<Vec<_>>()
                            .join(" -> "),
                    })
                })
            })
            .collect();
        if transitive.len() > MAX_TRANSITIVE {
            transitive.truncate(MAX_TRANSITIVE);
        }

        // Always add bounded TextMatch hits to reduce false negatives when the graph is incomplete.
        let exclude_chunk_id = graph.get_node(node).map(|nd| nd.chunk_id.as_str());
        let remaining = MAX_DIRECT.saturating_sub(direct.len());
        if remaining > 0 {
            for usage in Self::find_text_usages(chunks, &symbol, exclude_chunk_id, remaining) {
                let key = (usage.file.clone(), usage.line);
                if !seen_direct.insert(key) {
                    continue;
                }
                direct.push(usage);
                if direct.len() >= MAX_DIRECT {
                    break;
                }
            }
        }

        // Find related tests (deduplicated)
        let test_nodes = graph.find_related_tests(node);
        let mut tests: Vec<String> = test_nodes
            .iter()
            .filter_map(|n| {
                graph
                    .get_node(*n)
                    .map(|nd| format!("{}:{}", nd.symbol.file_path, nd.symbol.start_line))
            })
            .collect();
        tests.sort();
        tests.dedup();

        // Check if public API
        let public_api = graph.is_public_api(node);

        // Generate Mermaid diagram
        let mermaid = Self::generate_impact_mermaid(&symbol, &direct, &transitive);

        let total_usages = direct.len() + transitive.len();

        // Count unique files affected
        let files_affected: HashSet<&str> = direct
            .iter()
            .chain(transitive.iter())
            .map(|u| u.file.as_str())
            .collect();

        let result = ImpactResult {
            symbol,
            definition,
            total_usages,
            files_affected: files_affected.len(),
            direct,
            transitive,
            tests,
            public_api,
            mermaid,
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    fn find_text_usages(
        chunks: &[context_code_chunker::CodeChunk],
        symbol: &str,
        exclude_chunk_id: Option<&str>,
        max_results: usize,
    ) -> Vec<UsageInfo> {
        if symbol.is_empty() || max_results == 0 {
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut seen: HashSet<(String, usize)> = HashSet::new();

        for chunk in chunks {
            if out.len() >= max_results {
                break;
            }

            if chunk.file_path.ends_with(".md") {
                continue;
            }

            let chunk_id = format!(
                "{}:{}:{}",
                chunk.file_path, chunk.start_line, chunk.end_line
            );
            if let Some(exclude) = exclude_chunk_id {
                if chunk_id == exclude {
                    continue;
                }
            }

            let Some(hit_byte) = Self::find_word_boundary(&chunk.content, symbol) else {
                continue;
            };

            let line_offset = chunk.content[..hit_byte]
                .bytes()
                .filter(|b| *b == b'\n')
                .count();
            let line = chunk.start_line + line_offset;
            if !seen.insert((chunk.file_path.clone(), line)) {
                continue;
            }

            out.push(UsageInfo {
                file: chunk.file_path.clone(),
                line,
                symbol: chunk
                    .metadata
                    .symbol_name
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                relationship: "TextMatch".to_string(),
            });
        }

        out
    }

    fn find_word_boundary(haystack: &str, needle: &str) -> Option<usize> {
        if needle.is_empty() {
            return None;
        }

        let needle_is_ident = needle.bytes().all(Self::is_ident_byte);
        if !needle_is_ident {
            return haystack.find(needle);
        }

        let bytes = haystack.as_bytes();
        for (idx, _) in haystack.match_indices(needle) {
            let left_ok = idx == 0 || !Self::is_ident_byte(bytes[idx - 1]);
            let right_idx = idx + needle.len();
            let right_ok = right_idx >= bytes.len() || !Self::is_ident_byte(bytes[right_idx]);
            if left_ok && right_ok {
                return Some(idx);
            }
        }
        None
    }

    const fn is_ident_byte(b: u8) -> bool {
        matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
    }

    fn match_in_line(
        line: &str,
        pattern: &str,
        case_sensitive: bool,
        whole_word: bool,
    ) -> Option<usize> {
        if case_sensitive {
            if whole_word {
                Self::find_word_boundary(line, pattern)
            } else {
                line.find(pattern)
            }
        } else {
            let line_lower = line.to_ascii_lowercase();
            let pat_lower = pattern.to_ascii_lowercase();
            if whole_word {
                Self::find_word_boundary(&line_lower, &pat_lower)
            } else {
                line_lower.find(&pat_lower)
            }
        }
    }

    fn matches_file_pattern(path: &str, pattern: Option<&str>) -> bool {
        let Some(pattern) = pattern else {
            return true;
        };
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return true;
        }

        if !pattern.contains('*') && !pattern.contains('?') {
            return path.contains(pattern);
        }

        Self::glob_match(pattern, path)
    }

    // Minimal glob matcher supporting '*' and '?'.
    fn glob_match(pattern: &str, text: &str) -> bool {
        let p = pattern.as_bytes();
        let t = text.as_bytes();
        let mut p_idx = 0usize;
        let mut t_idx = 0usize;
        let mut star_idx: Option<usize> = None;
        let mut match_idx = 0usize;

        while t_idx < t.len() {
            if p_idx < p.len() && (p[p_idx] == b'?' || p[p_idx] == t[t_idx]) {
                p_idx += 1;
                t_idx += 1;
                continue;
            }

            if p_idx < p.len() && p[p_idx] == b'*' {
                star_idx = Some(p_idx);
                match_idx = t_idx;
                p_idx += 1;
                continue;
            }

            if let Some(star) = star_idx {
                p_idx = star + 1;
                match_idx += 1;
                t_idx = match_idx;
                continue;
            }

            return false;
        }

        while p_idx < p.len() && p[p_idx] == b'*' {
            p_idx += 1;
        }
        p_idx == p.len()
    }

    /// Trace call path between two symbols
    #[tool(
        description = "Show call chain from one symbol to another. Essential for understanding code flow and debugging."
    )]
    pub async fn trace(
        &self,
        Parameters(request): Parameters<TraceRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let root = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid path: {e}"
                ))]));
            }
        };

        let mut engine = match self.lock_engine(&root).await {
            Ok(engine) => engine,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {e}"
                ))]));
            }
        };

        let language = match request.language.as_deref() {
            Some(lang) => Self::parse_language(Some(lang)),
            None => Self::detect_language(engine.engine_mut().context_search.hybrid().chunks()),
        };

        if let Err(e) = engine.engine_mut().ensure_graph(language).await {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Graph build error: {e}"
            ))]));
        }

        let Some(assembler) = engine.engine_mut().context_search.assembler() else {
            return Ok(CallToolResult::error(vec![Content::text(
                "Graph build error: missing assembler after build",
            )]));
        };
        let graph = assembler.graph();

        // Find both symbols
        let from_node = match graph.find_node(&request.from) {
            Some(n) => n,
            None => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Symbol '{}' not found",
                    request.from
                ))]));
            }
        };

        let to_node = match graph.find_node(&request.to) {
            Some(n) => n,
            None => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Symbol '{}' not found",
                    request.to
                ))]));
            }
        };

        // Find path
        let path_with_edges = graph.find_path_with_edges(from_node, to_node);

        let (found, path_steps, depth) = match path_with_edges {
            Some(path) => {
                let steps: Vec<TraceStep> = path
                    .iter()
                    .map(|(n, rel)| {
                        let node_data = graph.get_node(*n);
                        TraceStep {
                            symbol: node_data
                                .map(|nd| nd.symbol.name.clone())
                                .unwrap_or_default(),
                            file: node_data
                                .map(|nd| nd.symbol.file_path.clone())
                                .unwrap_or_default(),
                            line: node_data.map(|nd| nd.symbol.start_line).unwrap_or(0),
                            relationship: rel.map(|r| format!("{:?}", r)),
                        }
                    })
                    .collect();
                let depth = steps.len().saturating_sub(1);
                (true, steps, depth)
            }
            None => (false, vec![], 0),
        };

        // Generate Mermaid sequence diagram
        let mermaid = Self::generate_trace_mermaid(&path_steps);

        let result = TraceResult {
            found,
            path: path_steps,
            depth,
            mermaid,
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Deep dive into a symbol
    #[tool(
        description = "Get complete information about a symbol: definition, dependencies, dependents, tests, and documentation."
    )]
    pub async fn explain(
        &self,
        Parameters(request): Parameters<ExplainRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let root = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid path: {e}"
                ))]));
            }
        };

        let mut engine = match self.lock_engine(&root).await {
            Ok(engine) => engine,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {e}"
                ))]));
            }
        };

        let language = match request.language.as_deref() {
            Some(lang) => Self::parse_language(Some(lang)),
            None => Self::detect_language(engine.engine_mut().context_search.hybrid().chunks()),
        };

        if let Err(e) = engine.engine_mut().ensure_graph(language).await {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Graph build error: {e}"
            ))]));
        }

        let Some(assembler) = engine.engine_mut().context_search.assembler() else {
            return Ok(CallToolResult::error(vec![Content::text(
                "Graph build error: missing assembler after build",
            )]));
        };
        let graph = assembler.graph();

        // Find the symbol
        let node = match graph.find_node(&request.symbol) {
            Some(n) => n,
            None => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Symbol '{}' not found",
                    request.symbol
                ))]));
            }
        };

        let node_data = graph.get_node(node);
        let (deps, dependents_raw) = graph.get_symbol_relations(node);

        // Format dependencies (filter unknown and markdown, deduplicate)
        let mut dependencies: Vec<String> = deps
            .iter()
            .filter_map(|(n, rel)| {
                graph.get_node(*n).and_then(|nd| {
                    if nd.symbol.name == "unknown" || nd.symbol.file_path.ends_with(".md") {
                        return None;
                    }
                    Some(format!("{} ({:?})", nd.symbol.name, rel))
                })
            })
            .collect();
        dependencies.sort();
        dependencies.dedup();

        // Format dependents (filter unknown and markdown, deduplicate)
        let mut dependents: Vec<String> = dependents_raw
            .iter()
            .filter_map(|(n, rel)| {
                graph.get_node(*n).and_then(|nd| {
                    if nd.symbol.name == "unknown" || nd.symbol.file_path.ends_with(".md") {
                        return None;
                    }
                    Some(format!("{} ({:?})", nd.symbol.name, rel))
                })
            })
            .collect();
        dependents.sort();
        dependents.dedup();

        // Find tests (deduplicated)
        let test_nodes = graph.find_related_tests(node);
        let mut tests: Vec<String> = test_nodes
            .iter()
            .filter_map(|n| graph.get_node(*n).map(|nd| nd.symbol.name.clone()))
            .collect();
        tests.sort();
        tests.dedup();

        // Get symbol info
        let (kind, file, line, documentation, content) = match node_data {
            Some(nd) => {
                let doc = nd
                    .chunk
                    .as_ref()
                    .and_then(|c| c.metadata.documentation.clone());
                let content = nd
                    .chunk
                    .as_ref()
                    .map(|c| c.content.clone())
                    .unwrap_or_default();
                (
                    format!("{:?}", nd.symbol.symbol_type),
                    nd.symbol.file_path.clone(),
                    nd.symbol.start_line,
                    doc,
                    content,
                )
            }
            None => (String::new(), String::new(), 0, None, String::new()),
        };

        let result = ExplainResult {
            symbol: request.symbol,
            kind,
            file,
            line,
            documentation,
            dependencies,
            dependents,
            tests,
            content,
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Project architecture overview
    #[tool(
        description = "Get project architecture snapshot: layers, entry points, key types, and graph statistics. Use this first to understand a new codebase."
    )]
    pub async fn overview(
        &self,
        Parameters(request): Parameters<OverviewRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));

        let root = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid path: {e}"
                ))]));
            }
        };

        let mut engine = match self.lock_engine(&root).await {
            Ok(engine) => engine,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {e}"
                ))]));
            }
        };

        let language = match request.language.as_deref() {
            Some(lang) => Self::parse_language(Some(lang)),
            None => {
                let chunks = engine.engine_mut().context_search.hybrid().chunks();
                Self::detect_language(chunks)
            }
        };

        if let Err(e) = engine.engine_mut().ensure_graph(language).await {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Graph build error: {e}"
            ))]));
        }

        let engine_ref = engine.engine_mut();
        let chunks = engine_ref.context_search.hybrid().chunks();
        let Some(assembler) = engine_ref.context_search.assembler() else {
            return Ok(CallToolResult::error(vec![Content::text(
                "Graph build error: missing assembler after build",
            )]));
        };
        let graph = assembler.graph();

        // Compute project info
        let total_files: HashSet<&str> = chunks.iter().map(|c| c.file_path.as_str()).collect();
        let total_lines: usize = chunks.iter().map(|c| c.content.lines().count()).sum();
        let project_name = root
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let project = ProjectInfo {
            name: project_name,
            files: total_files.len(),
            chunks: chunks.len(),
            lines: total_lines,
        };

        // Compute layers by top-level directory only (skip individual files at root)
        let mut layer_files: HashMap<String, HashSet<&str>> = HashMap::new();
        for chunk in chunks {
            let parts: Vec<&str> = chunk.file_path.split('/').collect();
            // Only use directories (skip root-level files)
            if parts.len() > 1 {
                let layer = parts.first().copied().unwrap_or("root").to_string();
                layer_files
                    .entry(layer)
                    .or_default()
                    .insert(&chunk.file_path);
            }
        }

        let mut layers: Vec<LayerInfo> = layer_files
            .into_iter()
            .map(|(name, files)| {
                let role = Self::guess_layer_role(&name);
                LayerInfo {
                    name,
                    files: files.len(),
                    role,
                }
            })
            .collect();
        // Sort by file count descending for better overview
        layers.sort_by(|a, b| b.files.cmp(&a.files));

        // Find entry points (filter unknown, tests, and deduplicate)
        let entry_nodes = graph.find_entry_points();
        let mut entry_points: Vec<String> = entry_nodes
            .iter()
            .filter_map(|n| {
                graph.get_node(*n).and_then(|nd| {
                    let name = &nd.symbol.name;
                    // Skip unknown, test functions, and markdown
                    if name == "unknown"
                        || name.starts_with("test_")
                        || nd.symbol.file_path.ends_with(".md")
                        || nd.symbol.file_path.contains("/tests/")
                    {
                        return None;
                    }
                    Some(name.clone())
                })
            })
            .collect();
        entry_points.sort();
        entry_points.dedup();
        entry_points.truncate(10);

        // Find key types (hotspots) - filter tests and deduplicate
        let hotspots = graph.find_hotspots(20); // Get more to filter
        let mut seen_names: HashSet<String> = HashSet::new();
        let key_types: Vec<KeyTypeInfo> = hotspots
            .iter()
            .filter_map(|(n, coupling)| {
                graph.get_node(*n).and_then(|nd| {
                    let name = &nd.symbol.name;
                    // Skip tests, unknown, duplicates
                    if name == "unknown"
                        || name == "tests"
                        || name.starts_with("test_")
                        || nd.symbol.file_path.contains("/tests/")
                        || seen_names.contains(name)
                    {
                        return None;
                    }
                    seen_names.insert(name.clone());
                    Some(KeyTypeInfo {
                        name: name.clone(),
                        kind: format!("{:?}", nd.symbol.symbol_type),
                        file: nd.symbol.file_path.clone(),
                        coupling: *coupling,
                    })
                })
            })
            .take(10)
            .collect();

        // Graph stats
        let (nodes, edges) = graph.stats();
        let graph_stats = GraphStats { nodes, edges };

        let result = OverviewResult {
            project,
            layers,
            entry_points,
            key_types,
            graph_stats,
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }
}

fn finalize_repo_onboarding_budget(result: &mut RepoOnboardingPackResult) -> anyhow::Result<()> {
    let mut used = 0usize;
    for _ in 0..8 {
        result.budget.used_chars = used;
        let raw = serde_json::to_string(result)?;
        let next = raw.chars().count();
        if next == used {
            result.budget.used_chars = next;
            return Ok(());
        }
        used = next;
    }

    result.budget.used_chars = used;
    Ok(())
}

fn compute_onboarding_doc_slice(
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

    let input_path = Path::new(file);
    let candidate = if input_path.is_absolute() {
        PathBuf::from(input_path)
    } else {
        root.join(input_path)
    };
    let canonical_file = candidate
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
        returned_lines,
        used_chars,
        max_lines,
        max_chars,
        truncated,
        truncation,
        file_size_bytes,
        file_mtime_ms,
        content_sha256,
        content,
    })
}

fn resolve_batch_refs(
    input: serde_json::Value,
    ctx: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    const MAX_DEPTH: usize = 64;

    fn decode_pointer_token(token: &str) -> Result<String, String> {
        let mut out = String::with_capacity(token.len());
        let mut chars = token.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '~' {
                out.push(ch);
                continue;
            }
            match chars.next() {
                Some('0') => out.push('~'),
                Some('1') => out.push('/'),
                Some(other) => return Err(format!("Invalid JSON pointer escape '~{other}'")),
                None => return Err("Invalid JSON pointer escape '~'".to_string()),
            }
        }
        Ok(out)
    }

    fn resolve_json_pointer<'a>(
        root: &'a serde_json::Value,
        pointer: &str,
    ) -> Result<&'a serde_json::Value, String> {
        let pointer = pointer.strip_prefix('#').unwrap_or(pointer);
        if pointer.is_empty() {
            return Ok(root);
        }
        if !pointer.starts_with('/') {
            return Err(format!(
                "$ref must be a JSON pointer starting with '#/' or '/': got {pointer:?}"
            ));
        }

        let mut tokens = Vec::new();
        for raw in pointer.split('/').skip(1) {
            tokens.push(decode_pointer_token(raw)?);
        }

        if tokens.len() >= 3 && tokens[0] == "items" && tokens[2] == "data" {
            if let Some(item) = root.get("items").and_then(|v| v.get(&tokens[1])) {
                if item.get("status").and_then(|v| v.as_str()) == Some("error") {
                    let msg = item
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    return Err(format!("$ref points to failed item '{}': {msg}", tokens[1]));
                }
            }
        }

        let mut current = root;
        for token in tokens {
            match current {
                serde_json::Value::Object(map) => {
                    current = map.get(&token).ok_or_else(|| {
                        format!("$ref path {pointer:?} not found at key {token:?}")
                    })?;
                }
                serde_json::Value::Array(arr) => {
                    let idx: usize = token.parse().map_err(|_| {
                        format!("$ref path {pointer:?} expected array index, got {token:?}")
                    })?;
                    current = arr.get(idx).ok_or_else(|| {
                        format!("$ref path {pointer:?} array index out of bounds: {idx}")
                    })?;
                }
                _ => {
                    return Err(format!(
                        "$ref path {pointer:?} reached non-container before token {token:?}"
                    ));
                }
            }
        }

        Ok(current)
    }

    fn resolve_inner(
        value: serde_json::Value,
        ctx: &serde_json::Value,
        depth: usize,
    ) -> Result<serde_json::Value, String> {
        if depth > MAX_DEPTH {
            return Err("Ref resolution exceeded max depth".to_string());
        }

        match value {
            serde_json::Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(resolve_inner(item, ctx, depth + 1)?);
                }
                Ok(serde_json::Value::Array(out))
            }
            serde_json::Value::Object(map) => {
                let default_value = map.get("$default").cloned();
                let is_ref_wrapper = map.contains_key("$ref")
                    && (map.len() == 1 || (map.len() == 2 && default_value.is_some()));

                if is_ref_wrapper {
                    let pointer = map
                        .get("$ref")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| "$ref must be a string".to_string())?;

                    match resolve_json_pointer(ctx, pointer) {
                        Ok(found) => resolve_inner(found.clone(), ctx, depth + 1),
                        Err(err) => {
                            if let Some(default) = default_value {
                                return resolve_inner(default, ctx, depth + 1);
                            }
                            Err(err)
                        }
                    }
                } else {
                    let mut out = serde_json::Map::new();
                    for (key, value) in map {
                        out.insert(key, resolve_inner(value, ctx, depth + 1)?);
                    }
                    Ok(serde_json::Value::Object(out))
                }
            }
            other => Ok(other),
        }
    }

    resolve_inner(input, ctx, 0)
}

fn extract_path_from_input(input: &serde_json::Value) -> Option<String> {
    let serde_json::Value::Object(map) = input else {
        return None;
    };
    map.get("path").and_then(|v| v.as_str()).map(str::to_string)
}

fn prepare_item_input(
    input: serde_json::Value,
    path: Option<&str>,
    tool: BatchToolName,
    remaining_chars: usize,
) -> serde_json::Value {
    let mut input = match input {
        serde_json::Value::Object(map) => serde_json::Value::Object(map),
        _ => serde_json::Value::Object(serde_json::Map::new()),
    };

    if let Some(path) = path {
        if let serde_json::Value::Object(ref mut map) = input {
            map.entry("path".to_string())
                .or_insert_with(|| serde_json::Value::String(path.to_string()));
        }
    }

    if matches!(
        tool,
        BatchToolName::ContextPack
            | BatchToolName::FileSlice
            | BatchToolName::ListFiles
            | BatchToolName::GrepContext
    ) {
        if let serde_json::Value::Object(ref mut map) = input {
            if !map.contains_key("max_chars") {
                let cap = remaining_chars.saturating_sub(300).clamp(1, 20_000);
                map.insert(
                    "max_chars".to_string(),
                    serde_json::Value::Number(cap.into()),
                );
            }
        }
    }

    input
}

fn parse_tool_result_as_json(
    result: &CallToolResult,
    tool: BatchToolName,
) -> Result<serde_json::Value, String> {
    if result.is_error.unwrap_or(false) {
        return Err(extract_tool_text(result).unwrap_or_else(|| "Tool returned error".to_string()));
    }

    if let Some(value) = result.structured_content.clone() {
        return Ok(value);
    }

    let blocks = extract_tool_text_blocks(result);
    if blocks.is_empty() {
        return Err("Tool returned no text content".to_string());
    }

    let mut parsed = Vec::new();
    for block in blocks {
        match serde_json::from_str::<serde_json::Value>(&block) {
            Ok(v) => parsed.push(v),
            Err(err) => {
                return Err(format!("Tool returned non-JSON text content: {err}"));
            }
        }
    }

    match parsed.len() {
        1 => Ok(parsed.remove(0)),
        2 if matches!(tool, BatchToolName::ContextPack) => Ok(serde_json::json!({
            "result": parsed[0],
            "trace": parsed[1],
        })),
        _ => Ok(serde_json::Value::Array(parsed)),
    }
}

fn extract_tool_text(result: &CallToolResult) -> Option<String> {
    let blocks = extract_tool_text_blocks(result);
    if blocks.is_empty() {
        return None;
    }
    Some(blocks.join("\n"))
}

fn extract_tool_text_blocks(result: &CallToolResult) -> Vec<String> {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect()
}

fn push_item_or_truncate(output: &mut BatchResult, item: BatchItemResult) -> bool {
    output.items.push(item);
    let used = match compute_used_chars(output) {
        Ok(used) => used,
        Err(err) => {
            let rejected = output.items.pop().expect("just pushed");
            output.budget.truncated = true;
            output.items.push(BatchItemResult {
                id: rejected.id,
                tool: rejected.tool,
                status: BatchItemStatus::Error,
                message: Some(format!("Failed to compute batch budget: {err:#}")),
                data: serde_json::Value::Null,
            });
            return false;
        }
    };

    if used > output.budget.max_chars {
        let rejected = output.items.pop().expect("just pushed");
        output.budget.truncated = true;

        if output.items.is_empty() {
            output.items.push(BatchItemResult {
                id: rejected.id,
                tool: rejected.tool,
                status: BatchItemStatus::Error,
                message: Some(format!(
                    "Batch budget exceeded (max_chars={}). Reduce payload sizes or raise max_chars.",
                    output.budget.max_chars
                )),
                data: serde_json::Value::Null,
            });
        }

        output.budget.used_chars = compute_used_chars(output).unwrap_or(output.budget.max_chars);
        return false;
    }

    output.budget.used_chars = used;
    true
}

fn compute_used_chars(output: &BatchResult) -> anyhow::Result<usize> {
    let mut tmp = BatchResult {
        version: output.version,
        items: output.items.clone(),
        budget: BatchBudget {
            max_chars: output.budget.max_chars,
            used_chars: 0,
            truncated: output.budget.truncated,
        },
    };
    let raw = serde_json::to_string(&tmp)?;
    let mut used = raw.chars().count();
    tmp.budget.used_chars = used;
    let raw = serde_json::to_string(&tmp)?;
    let next = raw.chars().count();
    if next == used {
        return Ok(used);
    }
    used = next;
    tmp.budget.used_chars = used;
    let raw = serde_json::to_string(&tmp)?;
    Ok(raw.chars().count())
}

fn hex_encode_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for b in bytes {
        write!(&mut out, "{:02x}", b).expect("write to String is infallible");
    }
    out
}

// ============================================================================
// Helper functions
// ============================================================================

impl ContextFinderService {
    fn parse_language(lang: Option<&str>) -> GraphLanguage {
        match lang {
            Some("python") => GraphLanguage::Python,
            Some("javascript") => GraphLanguage::JavaScript,
            Some("typescript") => GraphLanguage::TypeScript,
            _ => GraphLanguage::Rust,
        }
    }

    /// Auto-detect primary language from file extensions in chunks
    fn detect_language(chunks: &[context_code_chunker::CodeChunk]) -> GraphLanguage {
        let mut rust_count = 0;
        let mut python_count = 0;
        let mut js_count = 0;
        let mut ts_count = 0;

        for chunk in chunks {
            if chunk.file_path.ends_with(".rs") {
                rust_count += 1;
            } else if chunk.file_path.ends_with(".py") {
                python_count += 1;
            } else if chunk.file_path.ends_with(".ts") || chunk.file_path.ends_with(".tsx") {
                ts_count += 1;
            } else if chunk.file_path.ends_with(".js") || chunk.file_path.ends_with(".jsx") {
                js_count += 1;
            }
        }

        let max = rust_count.max(python_count).max(js_count).max(ts_count);
        if max == 0 {
            return GraphLanguage::Rust; // default
        }
        if max == rust_count {
            GraphLanguage::Rust
        } else if max == python_count {
            GraphLanguage::Python
        } else if max == ts_count {
            GraphLanguage::TypeScript
        } else {
            GraphLanguage::JavaScript
        }
    }

    fn guess_layer_role(name: &str) -> String {
        match name.to_lowercase().as_str() {
            "cli" | "cmd" | "bin" => "Command-line interface".to_string(),
            "api" | "server" | "web" => "API/Server layer".to_string(),
            "core" | "lib" | "src" => "Core library".to_string(),
            "test" | "tests" => "Test suite".to_string(),
            "crates" => "Workspace crates".to_string(),
            "docs" | "doc" => "Documentation".to_string(),
            _ => "Module".to_string(),
        }
    }

    fn generate_impact_mermaid(
        symbol: &str,
        direct: &[UsageInfo],
        transitive: &[UsageInfo],
    ) -> String {
        let mut lines = vec!["graph LR".to_string()];

        // Add direct edges
        for usage in direct.iter().take(10) {
            lines.push(format!(
                "    {}-->|{}|{}",
                Self::mermaid_safe(&usage.symbol),
                usage.relationship,
                Self::mermaid_safe(symbol)
            ));
        }

        // Add transitive edges (simplified)
        for usage in transitive.iter().take(5) {
            lines.push(format!(
                "    {}-.->|transitive|{}",
                Self::mermaid_safe(&usage.symbol),
                Self::mermaid_safe(symbol)
            ));
        }

        lines.join("\n")
    }

    fn generate_trace_mermaid(steps: &[TraceStep]) -> String {
        if steps.is_empty() {
            return "sequenceDiagram\n    Note over A: No path found".to_string();
        }

        let mut lines = vec!["sequenceDiagram".to_string()];

        for window in steps.windows(2) {
            let from = &window[0];
            let to = &window[1];
            let rel = to.relationship.as_deref().unwrap_or("calls");
            lines.push(format!(
                "    {}->>{}+: {}",
                Self::mermaid_safe(&from.symbol),
                Self::mermaid_safe(&to.symbol),
                rel
            ));
        }

        lines.join("\n")
    }

    fn mermaid_safe(s: &str) -> String {
        s.replace("::", "_").replace(['<', '>', ' '], "_")
    }
}

fn graph_language_key(language: GraphLanguage) -> &'static str {
    match language {
        GraphLanguage::Rust => "rust",
        GraphLanguage::Python => "python",
        GraphLanguage::JavaScript => "javascript",
        GraphLanguage::TypeScript => "typescript",
    }
}

fn pack_enriched_results(
    profile: &SearchProfile,
    enriched: Vec<context_search::EnrichedResult>,
    max_chars: usize,
    max_related_per_primary: usize,
) -> (Vec<ContextPackItem>, ContextPackBudget) {
    let mut used_chars = 0usize;
    let mut truncated = false;
    let mut dropped_items = 0usize;

    let mut items: Vec<ContextPackItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for er in enriched {
        let primary = er.primary;
        let primary_id = primary.id.clone();
        if !seen.insert(primary_id.clone()) {
            continue;
        }

        let primary_item = ContextPackItem {
            id: primary_id,
            role: "primary".to_string(),
            file: primary.chunk.file_path.clone(),
            start_line: primary.chunk.start_line,
            end_line: primary.chunk.end_line,
            symbol: primary.chunk.metadata.symbol_name.clone(),
            chunk_type: primary
                .chunk
                .metadata
                .chunk_type
                .map(|ct| ct.as_str().to_string()),
            score: primary.score,
            imports: primary.chunk.metadata.context_imports.clone(),
            content: primary.chunk.content,
            relationship: None,
            distance: None,
        };
        let cost = estimate_item_chars(&primary_item);
        if used_chars.saturating_add(cost) > max_chars {
            truncated = true;
            dropped_items += 1;
            break;
        }
        used_chars += cost;
        items.push(primary_item);

        let mut related = er.related;
        related.retain(|rc| !profile.is_rejected(&rc.chunk.file_path));
        related.sort_by(|a, b| {
            b.relevance_score
                .total_cmp(&a.relevance_score)
                .then_with(|| a.distance.cmp(&b.distance))
                .then_with(|| a.chunk.file_path.cmp(&b.chunk.file_path))
                .then_with(|| a.chunk.start_line.cmp(&b.chunk.start_line))
        });

        let relationship_cap = |kind: &str| -> usize {
            match kind {
                "Calls" => 6,
                "Uses" => 6,
                "Contains" => 4,
                "Extends" => 3,
                "Imports" => 2,
                "TestedBy" => 2,
                _ => 2,
            }
        };

        let mut selected_related = 0usize;
        let mut per_relationship: HashMap<String, usize> = HashMap::new();
        for rc in related {
            if selected_related >= max_related_per_primary {
                break;
            }

            let kind = rc
                .relationship_path
                .first()
                .cloned()
                .unwrap_or_else(String::new);
            let cap = relationship_cap(&kind);
            let used = per_relationship.get(kind.as_str()).copied().unwrap_or(0);
            if used >= cap {
                continue;
            }

            let id = format!(
                "{}:{}:{}",
                rc.chunk.file_path, rc.chunk.start_line, rc.chunk.end_line
            );
            if !seen.insert(id.clone()) {
                continue;
            }

            let item = ContextPackItem {
                id,
                role: "related".to_string(),
                file: rc.chunk.file_path.clone(),
                start_line: rc.chunk.start_line,
                end_line: rc.chunk.end_line,
                symbol: rc.chunk.metadata.symbol_name.clone(),
                chunk_type: rc
                    .chunk
                    .metadata
                    .chunk_type
                    .map(|ct| ct.as_str().to_string()),
                score: rc.relevance_score,
                imports: rc.chunk.metadata.context_imports.clone(),
                content: rc.chunk.content,
                relationship: Some(rc.relationship_path),
                distance: Some(rc.distance),
            };

            let cost = estimate_item_chars(&item);
            if used_chars.saturating_add(cost) > max_chars {
                truncated = true;
                dropped_items += 1;
                break;
            }
            used_chars += cost;
            items.push(item);
            *per_relationship.entry(kind).or_insert(0) += 1;
            selected_related += 1;
        }

        if truncated {
            break;
        }
    }

    (
        items,
        ContextPackBudget {
            max_chars,
            used_chars,
            truncated,
            dropped_items,
        },
    )
}

fn estimate_item_chars(item: &ContextPackItem) -> usize {
    let imports: usize = item.imports.iter().map(|s| s.len() + 1).sum();
    item.content.len() + imports + 128
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_code_chunker::ChunkMetadata;

    #[test]
    fn word_boundary_match_hits_only_whole_identifier() {
        assert!(ContextFinderService::find_word_boundary("fn new() {}", "new").is_some());
        assert!(ContextFinderService::find_word_boundary("renew", "new").is_none());
        assert!(ContextFinderService::find_word_boundary("news", "new").is_none());
        assert!(ContextFinderService::find_word_boundary("new_", "new").is_none());
        assert!(ContextFinderService::find_word_boundary(" new ", "new").is_some());
    }

    #[test]
    fn text_usages_compute_line_and_respect_exclusion() {
        let chunk = context_code_chunker::CodeChunk::new(
            "a.rs".to_string(),
            10,
            20,
            "fn caller() {\n  touch_daemon_best_effort();\n}\n".to_string(),
            ChunkMetadata::default()
                .symbol_name("caller")
                .chunk_type(context_code_chunker::ChunkType::Function),
        );

        let usages = ContextFinderService::find_text_usages(
            std::slice::from_ref(&chunk),
            "touch_daemon_best_effort",
            None,
            10,
        );
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].file, "a.rs");
        assert_eq!(usages[0].line, 11);
        assert_eq!(usages[0].symbol, "caller");
        assert_eq!(usages[0].relationship, "TextMatch");

        let exclude = format!(
            "{}:{}:{}",
            chunk.file_path, chunk.start_line, chunk.end_line
        );
        let excluded = ContextFinderService::find_text_usages(
            &[chunk],
            "touch_daemon_best_effort",
            Some(&exclude),
            10,
        );
        assert!(excluded.is_empty());
    }

    #[tokio::test]
    async fn map_works_without_index_and_has_no_side_effects() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let root_display = root.to_string_lossy().to_string();

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src").join("main.rs"),
            "fn main() { println!(\"hi\"); }\n",
        )
        .unwrap();

        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs").join("README.md"), "# Hello\n").unwrap();

        assert!(!root.join(".context-finder").exists());

        let result = compute_map_result(root, &root_display, 1, 20, 0)
            .await
            .unwrap();
        assert_eq!(result.total_files, 2);
        assert!(result.total_chunks > 0);
        assert!(result.directories.iter().any(|d| d.path == "src"));
        assert!(result.directories.iter().any(|d| d.path == "docs"));
        assert!(!result.truncated);
        assert!(result.next_cursor.is_none());

        // `map` must not create indexes/corpus.
        assert!(!root.join(".context-finder").exists());
    }

    #[tokio::test]
    async fn list_files_works_without_index_and_is_bounded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let root_display = root.to_string_lossy().to_string();

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();

        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs").join("README.md"), "# Hello\n").unwrap();

        std::fs::write(root.join("README.md"), "Root\n").unwrap();

        assert!(!root.join(".context-finder").exists());

        let result = compute_list_files_result(root, &root_display, None, 50, 20_000, None)
            .await
            .unwrap();
        assert_eq!(result.source, "filesystem");
        assert!(result.files.contains(&"src/main.rs".to_string()));
        assert!(result.files.contains(&"docs/README.md".to_string()));
        assert!(result.files.contains(&"README.md".to_string()));
        assert!(!result.truncated);
        assert!(result.next_cursor.is_none());

        let filtered =
            compute_list_files_result(root, &root_display, Some("docs"), 50, 20_000, None)
                .await
                .unwrap();
        assert_eq!(filtered.files, vec!["docs/README.md".to_string()]);
        assert!(!filtered.truncated);
        assert!(filtered.next_cursor.is_none());

        let globbed =
            compute_list_files_result(root, &root_display, Some("src/*"), 50, 20_000, None)
                .await
                .unwrap();
        assert_eq!(globbed.files, vec!["src/main.rs".to_string()]);
        assert!(!globbed.truncated);
        assert!(globbed.next_cursor.is_none());

        let limited = compute_list_files_result(root, &root_display, None, 1, 20_000, None)
            .await
            .unwrap();
        assert!(limited.truncated);
        assert_eq!(limited.truncation, Some(ListFilesTruncation::Limit));
        assert_eq!(limited.files.len(), 1);
        assert!(limited.next_cursor.is_some());

        let tiny = compute_list_files_result(root, &root_display, None, 50, 3, None)
            .await
            .unwrap();
        assert!(tiny.truncated);
        assert_eq!(tiny.truncation, Some(ListFilesTruncation::MaxChars));
        assert!(tiny.next_cursor.is_none());

        assert!(!root.join(".context-finder").exists());
    }

    #[test]
    fn batch_prepare_item_input_injects_max_chars_for_list_files() {
        let input = serde_json::json!({});
        let prepared = prepare_item_input(input, Some("/root"), BatchToolName::ListFiles, 5_000);

        let obj = prepared.as_object().expect("prepared input must be object");
        assert_eq!(obj.get("path").and_then(|v| v.as_str()), Some("/root"));
        assert!(
            obj.get("max_chars").is_some(),
            "expected max_chars to be injected for list_files"
        );
    }

    #[test]
    fn batch_prepare_item_input_injects_max_chars_for_grep_context() {
        let input = serde_json::json!({});
        let prepared = prepare_item_input(input, Some("/root"), BatchToolName::GrepContext, 5_000);

        let obj = prepared.as_object().expect("prepared input must be object");
        assert_eq!(obj.get("path").and_then(|v| v.as_str()), Some("/root"));
        assert!(
            obj.get("max_chars").is_some(),
            "expected max_chars to be injected for grep_context"
        );
    }

    #[tokio::test]
    async fn doctor_manifest_parsing_reports_missing_assets() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let model_dir = tmp.path().join("models");
        std::fs::create_dir_all(&model_dir).unwrap();

        std::fs::write(
            model_dir.join("manifest.json"),
            r#"{"schema_version":1,"models":[{"id":"m1","assets":[{"path":"m1/model.onnx"}]}]}"#,
        )
        .unwrap();

        let (exists, models) = load_model_statuses(&model_dir).await.unwrap();
        assert!(exists);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "m1");
        assert!(!models[0].installed);
        assert_eq!(models[0].missing_assets, vec!["m1/model.onnx"]);
    }

    #[tokio::test]
    async fn doctor_drift_helpers_detect_missing_and_extra_chunks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let corpus_path = tmp.path().join("corpus.json");
        let index_path = tmp.path().join("index.json");

        let mut corpus = ChunkCorpus::new();
        corpus.set_file_chunks(
            "a.rs".to_string(),
            vec![context_code_chunker::CodeChunk::new(
                "a.rs".to_string(),
                1,
                2,
                "alpha".to_string(),
                ChunkMetadata::default(),
            )],
        );
        corpus.set_file_chunks(
            "c.rs".to_string(),
            vec![context_code_chunker::CodeChunk::new(
                "c.rs".to_string(),
                10,
                12,
                "gamma".to_string(),
                ChunkMetadata::default(),
            )],
        );
        corpus.save(&corpus_path).await.unwrap();

        // Index contains one correct chunk id (a.rs:1:2) and one extra (b.rs:1:1),
        // while missing c.rs:10:12.
        std::fs::write(
            &index_path,
            r#"{"schema_version":3,"dimension":384,"next_id":2,"id_map":{"0":"a.rs:1:2","1":"b.rs:1:1"},"vectors":{}}"#,
        )
        .unwrap();

        let corpus_ids = load_corpus_chunk_ids(&corpus_path).await.unwrap();
        let index_ids = load_index_chunk_ids(&index_path).await.unwrap();

        assert_eq!(corpus_ids.len(), 2);
        assert_eq!(index_ids.len(), 2);
        assert_eq!(corpus_ids.difference(&index_ids).count(), 1);
        assert_eq!(index_ids.difference(&corpus_ids).count(), 1);
    }
}
