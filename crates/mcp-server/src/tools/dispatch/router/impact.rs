use super::super::{
    AutoIndexPolicy, CallToolResult, Content, ContextFinderService, ImpactRequest, ImpactResult,
    McpError, SymbolLocation, UsageInfo,
};
use crate::tools::util::path_has_extension_ignore_ascii_case;
use context_code_chunker::CodeChunk;
use context_graph::CodeGraph;
use petgraph::graph::NodeIndex;
use std::collections::HashSet;

const MAX_DIRECT: usize = 200;
const MAX_TRANSITIVE: usize = 200;

fn success_payload(result: &ImpactResult) -> CallToolResult {
    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(result).unwrap_or_default(),
    )])
}

fn best_effort_text_only(symbol: String, chunks: &[CodeChunk]) -> ImpactResult {
    let direct = ContextFinderService::find_text_usages(chunks, &symbol, None, MAX_DIRECT);
    let mermaid = ContextFinderService::generate_impact_mermaid(&symbol, &direct, &[]);
    let files_affected: HashSet<&str> = direct.iter().map(|u| u.file.as_str()).collect();

    ImpactResult {
        symbol,
        definition: None,
        total_usages: direct.len(),
        files_affected: files_affected.len(),
        direct,
        transitive: Vec::new(),
        tests: Vec::new(),
        public_api: false,
        mermaid,
        meta: None,
    }
}

fn should_skip_graph_symbol(symbol_name: &str, file_path: &str) -> bool {
    symbol_name == "unknown" || path_has_extension_ignore_ascii_case(file_path, "md")
}

fn collect_direct_usages(
    graph: &CodeGraph,
    node: NodeIndex,
) -> (Vec<UsageInfo>, HashSet<(String, usize)>) {
    let direct_usages = graph.get_all_usages(node);
    let mut seen: HashSet<(String, usize)> = HashSet::new();
    let mut direct: Vec<UsageInfo> = direct_usages
        .iter()
        .filter_map(|(n, rel)| {
            graph.get_node(*n).and_then(|nd| {
                if should_skip_graph_symbol(&nd.symbol.name, &nd.symbol.file_path) {
                    return None;
                }
                let key = (nd.symbol.file_path.clone(), nd.symbol.start_line);
                if !seen.insert(key) {
                    return None;
                }
                Some(UsageInfo {
                    file: nd.symbol.file_path.clone(),
                    line: nd.symbol.start_line,
                    symbol: nd.symbol.name.clone(),
                    relationship: format!("{rel:?}"),
                })
            })
        })
        .collect();

    direct.truncate(MAX_DIRECT);
    (direct, seen)
}

fn collect_transitive_usages(graph: &CodeGraph, node: NodeIndex, depth: usize) -> Vec<UsageInfo> {
    let transitive_usages = graph.get_transitive_usages(node, depth);
    let mut seen: HashSet<(String, usize)> = HashSet::new();
    let mut transitive: Vec<UsageInfo> = transitive_usages
        .iter()
        .filter(|(_, d, _)| *d > 1)
        .filter_map(|(n, _, path)| {
            graph.get_node(*n).and_then(|nd| {
                if should_skip_graph_symbol(&nd.symbol.name, &nd.symbol.file_path) {
                    return None;
                }
                let key = (nd.symbol.file_path.clone(), nd.symbol.start_line);
                if !seen.insert(key) {
                    return None;
                }
                Some(UsageInfo {
                    file: nd.symbol.file_path.clone(),
                    line: nd.symbol.start_line,
                    symbol: nd.symbol.name.clone(),
                    relationship: path
                        .iter()
                        .map(|r| format!("{r:?}"))
                        .collect::<Vec<_>>()
                        .join(" -> "),
                })
            })
        })
        .collect();

    transitive.truncate(MAX_TRANSITIVE);
    transitive
}

fn add_text_hits_to_direct(
    direct: &mut Vec<UsageInfo>,
    seen_direct: &mut HashSet<(String, usize)>,
    chunks: &[CodeChunk],
    symbol: &str,
    exclude_chunk_id: Option<&str>,
) {
    let remaining = MAX_DIRECT.saturating_sub(direct.len());
    if remaining == 0 {
        return;
    }

    for usage in ContextFinderService::find_text_usages(chunks, symbol, exclude_chunk_id, remaining)
    {
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

fn collect_related_tests(graph: &CodeGraph, node: NodeIndex) -> Vec<String> {
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
    tests
}

fn count_files_affected(direct: &[UsageInfo], transitive: &[UsageInfo]) -> usize {
    direct
        .iter()
        .chain(transitive.iter())
        .map(|u| u.file.as_str())
        .collect::<HashSet<_>>()
        .len()
}

/// Find all usages of a symbol (impact analysis)
pub(in crate::tools::dispatch) async fn impact(
    service: &ContextFinderService,
    request: ImpactRequest,
) -> Result<CallToolResult, McpError> {
    let depth = request.depth.unwrap_or(2).clamp(1, 3);
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

    let symbol = request.symbol;
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

    let graph_ready = engine.engine_mut().ensure_graph(language).await.is_ok();

    let mut result = if graph_ready {
        let engine_ref = engine.engine_mut();
        let chunks = engine_ref.context_search.hybrid().chunks();

        match engine_ref.context_search.assembler() {
            None => best_effort_text_only(symbol, chunks),
            Some(assembler) => {
                let graph = assembler.graph();
                match graph.find_node(&symbol) {
                    None => best_effort_text_only(symbol, chunks),
                    Some(node) => {
                        let definition = graph.get_node(node).map(|nd| SymbolLocation {
                            file: nd.symbol.file_path.clone(),
                            line: nd.symbol.start_line,
                        });

                        let (mut direct, mut seen_direct) = collect_direct_usages(graph, node);

                        let transitive = if depth > 1 {
                            collect_transitive_usages(graph, node, depth)
                        } else {
                            Vec::new()
                        };

                        let exclude_chunk_id = graph.get_node(node).map(|nd| nd.chunk_id.as_str());
                        add_text_hits_to_direct(
                            &mut direct,
                            &mut seen_direct,
                            chunks,
                            &symbol,
                            exclude_chunk_id,
                        );

                        let tests = collect_related_tests(graph, node);
                        let public_api = graph.is_public_api(node);
                        let mermaid = ContextFinderService::generate_impact_mermaid(
                            &symbol,
                            &direct,
                            &transitive,
                        );
                        let total_usages = direct.len() + transitive.len();

                        ImpactResult {
                            symbol,
                            definition,
                            total_usages,
                            files_affected: count_files_affected(&direct, &transitive),
                            direct,
                            transitive,
                            tests,
                            public_api,
                            mermaid,
                            meta: None,
                        }
                    }
                }
            }
        }
    } else {
        let chunks = engine.engine_mut().context_search.hybrid().chunks();
        best_effort_text_only(symbol, chunks)
    };

    drop(engine);
    result.meta = Some(meta);
    Ok(success_payload(&result))
}
