use super::super::{
    AutoIndexPolicy, CallToolResult, Content, ContextFinderService, GraphStats, KeyTypeInfo,
    LayerInfo, McpError, OverviewRequest, OverviewResult, ProjectInfo, ResponseMode, ToolMeta,
};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::paths::normalize_relative_path;
use crate::tools::secrets::is_potential_secret_path;
use crate::tools::util::path_has_extension_ignore_ascii_case;
use context_code_chunker::CodeChunk;
use context_graph::CodeGraph;
use context_indexer::FileScanner;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_request_with_root_context,
    meta_for_request,
};
use super::semantic_fallback::is_semantic_unavailable_error;
const MAX_ENTRY_POINTS: usize = 10;
const MAX_KEY_TYPES: usize = 10;
const HOTSPOT_LIMIT: usize = 20;
const MAX_FALLBACK_LINE_COUNT_FILES: usize = 200;
const MAX_FALLBACK_LINE_COUNT_BYTES_TOTAL: u64 = 2_000_000;
const MAX_FALLBACK_LINE_COUNT_BYTES_PER_FILE: u64 = 200_000;

fn looks_like_text_file(file_path: &str) -> bool {
    let lower = file_path.to_ascii_lowercase();
    // Keep this minimal: just avoid obvious binaries so line counts are meaningful.
    !matches!(
        lower.as_str(),
        _ if lower.ends_with(".png")
            || lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".gif")
            || lower.ends_with(".ico")
            || lower.ends_with(".pdf")
            || lower.ends_with(".zip")
            || lower.ends_with(".gz")
            || lower.ends_with(".tar")
            || lower.ends_with(".tgz")
            || lower.ends_with(".7z")
            || lower.ends_with(".jar")
            || lower.ends_with(".class")
            || lower.ends_with(".so")
            || lower.ends_with(".dylib")
            || lower.ends_with(".dll")
            || lower.ends_with(".exe")
            || lower.ends_with(".bin")
            || lower.ends_with(".wasm")
            || lower.ends_with(".mp4")
            || lower.ends_with(".mp3")
            || lower.ends_with(".wav")
            || lower.ends_with(".woff")
            || lower.ends_with(".woff2")
            || lower.ends_with(".ttf")
            || lower.ends_with(".otf")
    )
}

fn looks_like_entry_file(file_path: &str) -> bool {
    let path = file_path.trim();
    if path.is_empty() {
        return false;
    }
    if path.contains("/tests/") || path.contains("/test/") {
        return false;
    }
    if path_has_extension_ignore_ascii_case(path, "md")
        || path_has_extension_ignore_ascii_case(path, "mdx")
    {
        return false;
    }

    // Keep this simple and language-agnostic: prioritize obvious entry points that appear
    // across ecosystems.
    let lower = path.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        // Rust
        _ if lower.ends_with("src/main.rs")
            || lower.ends_with("src/lib.rs")
            // Python
            || lower.ends_with("src/__main__.py")
            || lower.ends_with("src/main.py")
            // JS/TS
            || lower.ends_with("src/index.ts")
            || lower.ends_with("src/index.tsx")
            || lower.ends_with("src/index.js")
            || lower.ends_with("src/index.jsx")
            || lower.ends_with("src/main.ts")
            || lower.ends_with("src/main.js")
            // Go
            || lower.ends_with("main.go")
    )
}

fn compute_layers_from_files(files: &[String]) -> Vec<LayerInfo> {
    let mut layer_files: HashMap<String, HashSet<&str>> = HashMap::new();
    for file in files {
        let parts: Vec<&str> = file.split('/').collect();
        if parts.len() > 1 {
            let layer = parts.first().copied().unwrap_or("root").to_string();
            layer_files.entry(layer).or_default().insert(file.as_str());
        }
    }

    let mut layers: Vec<LayerInfo> = layer_files
        .into_iter()
        .map(|(name, files)| {
            let role = ContextFinderService::guess_layer_role(&name);
            LayerInfo {
                name,
                files: files.len(),
                role,
            }
        })
        .collect();
    layers.sort_by(|a, b| b.files.cmp(&a.files));
    layers
}

fn compute_entry_points_from_files(files: &[String]) -> Vec<String> {
    let mut entry_points: Vec<String> = files
        .iter()
        .map(|s| s.as_str())
        .filter(|p| looks_like_entry_file(p))
        .map(|p| p.to_string())
        .collect();

    entry_points.sort();
    entry_points.dedup();
    entry_points.truncate(MAX_ENTRY_POINTS);
    entry_points
}

fn count_lines_in_bytes(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let newline_count = bytes.iter().filter(|b| **b == b'\n').count();
    // Match `.lines().count()` semantics: include the final line even if there's no trailing '\n'.
    if bytes.last() == Some(&b'\n') {
        newline_count
    } else {
        newline_count + 1
    }
}

fn compute_total_lines_bounded(root: &Path, files: &[String]) -> usize {
    let mut total_lines = 0usize;
    let mut total_bytes = 0u64;
    let mut counted_files = 0usize;

    for file in files {
        if counted_files >= MAX_FALLBACK_LINE_COUNT_FILES {
            break;
        }
        if !looks_like_text_file(file) {
            continue;
        }
        let path = root.join(file);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let len = meta.len();
        if len == 0 || len > MAX_FALLBACK_LINE_COUNT_BYTES_PER_FILE {
            continue;
        }
        if total_bytes.saturating_add(len) > MAX_FALLBACK_LINE_COUNT_BYTES_TOTAL {
            break;
        }

        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        total_lines = total_lines.saturating_add(count_lines_in_bytes(&bytes));
        total_bytes = total_bytes.saturating_add(len);
        counted_files += 1;
    }

    total_lines
}

fn compute_filesystem_overview(root: &Path) -> (ProjectInfo, Vec<LayerInfo>, Vec<String>) {
    let name = root.file_name().map_or_else(
        || "unknown".to_string(),
        |s| s.to_string_lossy().to_string(),
    );

    let scanner = FileScanner::new(root);
    let scanned_paths = scanner.scan();
    let mut files: Vec<String> = scanned_paths
        .into_iter()
        .filter_map(|p| normalize_relative_path(root, &p))
        .filter(|p| !is_potential_secret_path(p))
        .collect();
    files.sort();

    let lines = compute_total_lines_bounded(root, &files);

    let project = ProjectInfo {
        name,
        files: files.len(),
        // Best-effort: without semantic index we treat each file as a single chunk for sizing.
        chunks: files.len(),
        lines,
    };

    let layers = compute_layers_from_files(&files);
    let entry_points = compute_entry_points_from_files(&files);
    (project, layers, entry_points)
}

fn compute_project_info(root: &Path, chunks: &[CodeChunk]) -> ProjectInfo {
    let total_files: HashSet<&str> = chunks.iter().map(|c| c.file_path.as_str()).collect();
    let total_lines: usize = chunks.iter().map(|c| c.content.lines().count()).sum();
    let name = root.file_name().map_or_else(
        || "unknown".to_string(),
        |s| s.to_string_lossy().to_string(),
    );

    ProjectInfo {
        name,
        files: total_files.len(),
        chunks: chunks.len(),
        lines: total_lines,
    }
}

fn compute_layers(chunks: &[CodeChunk]) -> Vec<LayerInfo> {
    let mut layer_files: HashMap<String, HashSet<&str>> = HashMap::new();
    for chunk in chunks {
        let parts: Vec<&str> = chunk.file_path.split('/').collect();
        if parts.len() > 1 {
            let layer = parts.first().copied().unwrap_or("root").to_string();
            layer_files
                .entry(layer)
                .or_default()
                .insert(chunk.file_path.as_str());
        }
    }

    let mut layers: Vec<LayerInfo> = layer_files
        .into_iter()
        .map(|(name, files)| {
            let role = ContextFinderService::guess_layer_role(&name);
            LayerInfo {
                name,
                files: files.len(),
                role,
            }
        })
        .collect();

    layers.sort_by(|a, b| b.files.cmp(&a.files));
    layers
}

fn compute_entry_points(chunks: &[CodeChunk]) -> Vec<String> {
    let mut entry_points: Vec<String> = chunks
        .iter()
        .map(|c| c.file_path.as_str())
        .filter(|p| looks_like_entry_file(p))
        .map(|p| p.to_string())
        .collect();

    entry_points.sort();
    entry_points.dedup();
    entry_points.truncate(MAX_ENTRY_POINTS);
    entry_points
}

fn compute_key_types(graph: &CodeGraph) -> Vec<KeyTypeInfo> {
    let hotspots = graph.find_hotspots(HOTSPOT_LIMIT);
    let mut seen_names: HashSet<String> = HashSet::new();

    hotspots
        .iter()
        .filter_map(|(n, coupling)| {
            graph.get_node(*n).and_then(|nd| {
                let name = &nd.symbol.name;
                if name == "unknown"
                    || name == "tests"
                    || name.starts_with("test_")
                    || nd.symbol.file_path.contains("/tests/")
                    || !seen_names.insert(name.clone())
                {
                    return None;
                }

                let symbol_type = &nd.symbol.symbol_type;
                Some(KeyTypeInfo {
                    name: name.clone(),
                    kind: format!("{symbol_type:?}"),
                    file: nd.symbol.file_path.clone(),
                    coupling: *coupling,
                })
            })
        })
        .take(MAX_KEY_TYPES)
        .collect()
}

/// Project architecture overview
pub(in crate::tools::dispatch) async fn overview(
    service: &ContextFinderService,
    request: OverviewRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let root = match service
        .resolve_root_for_tool(request.path.as_deref(), "overview")
        .await
    {
        Ok((root, _)) => root,
        Err(message) => {
            let meta = if response_mode == ResponseMode::Minimal {
                ToolMeta::default()
            } else {
                meta_for_request(service, request.path.as_deref()).await
            };
            return Ok(
                invalid_request_with_root_context(service, message, meta, None, Vec::new()).await,
            );
        }
    };
    let policy = AutoIndexPolicy::from_request(request.auto_index, request.auto_index_budget_ms);
    let (mut engine, meta) = match service.prepare_semantic_engine(&root, policy).await {
        Ok(engine) => engine,
        Err(e) => {
            let message = format!("Error: {e}");
            let meta = service.tool_meta(&root).await;
            let meta_for_output = if response_mode == ResponseMode::Minimal {
                ToolMeta {
                    root_fingerprint: meta.root_fingerprint,
                    ..ToolMeta::default()
                }
            } else {
                meta
            };
            if is_semantic_unavailable_error(&message) {
                let (project, layers, entry_points) = compute_filesystem_overview(&root);
                let result = OverviewResult {
                    project,
                    layers,
                    entry_points,
                    key_types: Vec::new(),
                    graph_stats: GraphStats { nodes: 0, edges: 0 },
                    meta: meta_for_output.clone(),
                };

                let mut doc = ContextDocBuilder::new();
                let answer = if response_mode == ResponseMode::Full {
                    "overview: best-effort (filesystem fallback)"
                } else {
                    "overview: best-effort"
                };
                doc.push_answer(answer);
                if response_mode != ResponseMode::Minimal {
                    doc.push_root_fingerprint(result.meta.root_fingerprint);
                }
                if response_mode != ResponseMode::Minimal {
                    doc.push_note("semantic: unavailable (filesystem fallback)");
                }
                if !result.layers.is_empty() {
                    doc.push_note("layers:");
                    for layer in &result.layers {
                        doc.push_line(&format!(
                            "- {} (files={}, role={})",
                            layer.name, layer.files, layer.role
                        ));
                    }
                }
                if !result.entry_points.is_empty() {
                    doc.push_note("entry_points:");
                    for ep in &result.entry_points {
                        doc.push_line(&format!("- {ep}"));
                    }
                }

                let output = CallToolResult::success(vec![Content::text(doc.finish())]);
                return Ok(attach_structured_content(
                    output,
                    &result,
                    result.meta.clone(),
                    "overview",
                ));
            }

            return Ok(internal_error_with_meta(message, meta_for_output));
        }
    };
    let meta_for_output = if response_mode == ResponseMode::Minimal {
        ToolMeta {
            root_fingerprint: meta.root_fingerprint,
            ..ToolMeta::default()
        }
    } else {
        meta.clone()
    };

    let detected_language = {
        let chunks = engine.engine_mut().context_search.hybrid().chunks();
        ContextFinderService::detect_language(chunks)
    };
    let language = request
        .language
        .as_deref()
        .map_or(detected_language, |lang| {
            ContextFinderService::parse_language(Some(lang))
        });

    let result = {
        let engine_ref = engine.engine_mut();
        let chunks = engine_ref.context_search.hybrid().chunks();

        let project = compute_project_info(&root, chunks);
        let layers = compute_layers(chunks);
        let entry_points = compute_entry_points(chunks);

        // Agent-first UX: `overview` should be useful even when graph building is unavailable or
        // undesirable. We only build the graph in `full` mode.
        let (key_types, graph_stats) = if response_mode == ResponseMode::Full {
            if let Err(_e) = engine_ref.ensure_graph(language).await {
                (Vec::new(), GraphStats { nodes: 0, edges: 0 })
            } else if let Some(assembler) = engine_ref.context_search.assembler() {
                let graph = assembler.graph();
                let key_types = compute_key_types(graph);
                let (nodes, edges) = graph.stats();
                (key_types, GraphStats { nodes, edges })
            } else {
                (Vec::new(), GraphStats { nodes: 0, edges: 0 })
            }
        } else {
            (Vec::new(), GraphStats { nodes: 0, edges: 0 })
        };

        OverviewResult {
            project,
            layers,
            entry_points,
            key_types,
            graph_stats,
            meta: meta_for_output,
        }
    };

    drop(engine);

    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!(
        "overview: {} (files={}, chunks={}, lines={})",
        result.project.name, result.project.files, result.project.chunks, result.project.lines
    ));
    if response_mode != ResponseMode::Minimal {
        doc.push_root_fingerprint(result.meta.root_fingerprint);
    }
    if !result.layers.is_empty() {
        doc.push_note("layers:");
        for layer in &result.layers {
            doc.push_line(&format!(
                "- {} (files={}, role={})",
                layer.name, layer.files, layer.role
            ));
        }
    }
    if !result.entry_points.is_empty() {
        doc.push_note("entry_points:");
        for ep in &result.entry_points {
            doc.push_line(&format!("- {ep}"));
        }
    }
    if response_mode == ResponseMode::Full {
        if !result.key_types.is_empty() {
            doc.push_note("key_types:");
            for kt in &result.key_types {
                doc.push_line(&format!("- {} ({}) {}", kt.name, kt.kind, kt.file));
            }
        }
        doc.push_note(&format!(
            "graph: nodes={} edges={}",
            result.graph_stats.nodes, result.graph_stats.edges
        ));
    }
    let output = CallToolResult::success(vec![Content::text(doc.finish())]);
    Ok(attach_structured_content(
        output,
        &result,
        result.meta.clone(),
        "overview",
    ))
}
