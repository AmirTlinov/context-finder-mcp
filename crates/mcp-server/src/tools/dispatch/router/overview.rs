use super::super::{
    AutoIndexPolicy, CallToolResult, Content, ContextFinderService, GraphStats, KeyTypeInfo,
    LayerInfo, McpError, OverviewRequest, OverviewResult, ProjectInfo,
};
use crate::tools::util::path_has_extension_ignore_ascii_case;
use context_code_chunker::CodeChunk;
use context_graph::CodeGraph;
use std::collections::{HashMap, HashSet};
use std::path::Path;

const MAX_ENTRY_POINTS: usize = 10;
const MAX_KEY_TYPES: usize = 10;
const HOTSPOT_LIMIT: usize = 20;

fn success_payload(result: &OverviewResult) -> CallToolResult {
    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(result).unwrap_or_default(),
    )])
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

fn is_entry_point_candidate(symbol_name: &str, file_path: &str) -> bool {
    if symbol_name == "unknown" || symbol_name.starts_with("test_") {
        return false;
    }
    if file_path.contains("/tests/") || path_has_extension_ignore_ascii_case(file_path, "md") {
        return false;
    }
    true
}

fn compute_entry_points(graph: &CodeGraph) -> Vec<String> {
    let entry_nodes = graph.find_entry_points();
    let mut entry_points: Vec<String> = entry_nodes
        .iter()
        .filter_map(|n| {
            graph.get_node(*n).and_then(|nd| {
                is_entry_point_candidate(&nd.symbol.name, &nd.symbol.file_path)
                    .then(|| nd.symbol.name.clone())
            })
        })
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
    let root = match service.resolve_root(request.path.as_deref()).await {
        Ok((root, _)) => root,
        Err(message) => {
            return Ok(CallToolResult::error(vec![Content::text(message)]));
        }
    };

    let policy = AutoIndexPolicy::from_request(request.auto_index, request.auto_index_budget_ms);
    let (mut engine, meta) = match service.prepare_semantic_engine(&root, policy).await {
        Ok(engine) => engine,
        Err(e) => {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: {e}"
            ))]));
        }
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

    if let Err(e) = engine.engine_mut().ensure_graph(language).await {
        return Ok(CallToolResult::error(vec![Content::text(format!(
            "Graph build error: {e}"
        ))]));
    }

    let result = {
        let engine_ref = engine.engine_mut();
        let chunks = engine_ref.context_search.hybrid().chunks();
        let Some(assembler) = engine_ref.context_search.assembler() else {
            return Ok(CallToolResult::error(vec![Content::text(
                "Graph build error: missing assembler after build",
            )]));
        };
        let graph = assembler.graph();

        let project = compute_project_info(&root, chunks);
        let layers = compute_layers(chunks);
        let entry_points = compute_entry_points(graph);
        let key_types = compute_key_types(graph);

        let (nodes, edges) = graph.stats();
        let graph_stats = GraphStats { nodes, edges };

        OverviewResult {
            project,
            layers,
            entry_points,
            key_types,
            graph_stats,
            meta: Some(meta),
        }
    };

    drop(engine);
    Ok(success_payload(&result))
}
