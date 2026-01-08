use super::super::{
    AutoIndexPolicy, CallToolResult, Content, ContextFinderService, GraphStats, KeyTypeInfo,
    LayerInfo, McpError, OverviewRequest, OverviewResult, ProjectInfo, ResponseMode, ToolMeta,
};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::util::path_has_extension_ignore_ascii_case;
use context_code_chunker::CodeChunk;
use context_graph::CodeGraph;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_request_with_meta,
    meta_for_request,
};
const MAX_ENTRY_POINTS: usize = 10;
const MAX_KEY_TYPES: usize = 10;
const HOTSPOT_LIMIT: usize = 20;

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
            _ if lower.ends_with("/src/main.rs")
                || lower.ends_with("/src/lib.rs")
                // Python
                || lower.ends_with("/src/__main__.py")
                || lower.ends_with("/src/main.py")
                // JS/TS
                || lower.ends_with("/src/index.ts")
                || lower.ends_with("/src/index.tsx")
                || lower.ends_with("/src/index.js")
                || lower.ends_with("/src/index.jsx")
                || lower.ends_with("/src/main.ts")
                || lower.ends_with("/src/main.js")
                // Go
                || lower.ends_with("/main.go")
        )
    }

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
    let root = match service.resolve_root(request.path.as_deref()).await {
        Ok((root, _)) => root,
        Err(message) => {
            let meta = if response_mode == ResponseMode::Minimal {
                ToolMeta::default()
            } else {
                meta_for_request(service, request.path.as_deref()).await
            };
            return Ok(invalid_request_with_meta(message, meta, None, Vec::new()));
        }
    };
    let policy = AutoIndexPolicy::from_request(request.auto_index, request.auto_index_budget_ms);
    let (mut engine, meta) = match service.prepare_semantic_engine(&root, policy).await {
        Ok(engine) => engine,
        Err(e) => {
            let meta = service.tool_meta(&root).await;
            let meta_for_output = if response_mode == ResponseMode::Minimal {
                ToolMeta {
                    root_fingerprint: meta.root_fingerprint,
                    ..ToolMeta::default()
                }
            } else {
                meta
            };
            return Ok(internal_error_with_meta(
                format!("Error: {e}"),
                meta_for_output,
            ));
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
    doc.push_root_fingerprint(result.meta.root_fingerprint);
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
