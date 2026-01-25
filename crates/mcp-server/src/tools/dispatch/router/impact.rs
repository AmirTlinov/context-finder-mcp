use super::super::{
    AutoIndexPolicy, CallToolResult, Content, ContextFinderService, ImpactRequest, ImpactResult,
    McpError, ResponseMode, SymbolLocation, UsageInfo,
};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::util::path_has_extension_ignore_ascii_case;
use context_code_chunker::CodeChunk;
use context_graph::CodeGraph;
use context_indexer::ToolMeta;
use petgraph::graph::NodeIndex;
use std::collections::HashSet;

use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_request_with_root_context,
    meta_for_request,
};
use super::semantic_fallback::{grep_fallback_hunks, is_semantic_unavailable_error};
const MAX_DIRECT: usize = 200;
const MAX_TRANSITIVE: usize = 200;

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
        meta: ToolMeta::default(),
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
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let depth = request.depth.unwrap_or(2).clamp(1, 3);
    let (root, root_display) = match service
        .resolve_root_for_tool(request.path.as_deref(), "impact")
        .await
    {
        Ok((root, root_display)) => (root, root_display),
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
    let (mut engine, meta) = match service
        .prepare_semantic_engine_for_query(&root, policy, &request.symbol)
        .await
    {
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
                let budgets = super::super::mcp_default_budgets();
                let max_hunks = 12usize;
                let symbol = request.symbol.clone();
                let hunks = match grep_fallback_hunks(
                    &root,
                    &root_display,
                    &symbol,
                    response_mode,
                    max_hunks,
                    budgets.grep_context_max_chars,
                )
                .await
                {
                    Ok(hunks) => hunks,
                    Err(err) => {
                        return Ok(internal_error_with_meta(
                            format!("{message} (fallback grep failed: {err:#})"),
                            meta_for_output,
                        ));
                    }
                };

                let direct: Vec<UsageInfo> = hunks
                    .iter()
                    .map(|hunk| UsageInfo {
                        file: hunk.file.clone(),
                        line: hunk.start_line,
                        symbol: symbol.clone(),
                        relationship: "TextMatch".to_string(),
                    })
                    .collect();

                let files_affected = direct
                    .iter()
                    .map(|usage| usage.file.as_str())
                    .collect::<HashSet<_>>()
                    .len();

                let result = ImpactResult {
                    symbol,
                    definition: None,
                    total_usages: direct.len(),
                    files_affected,
                    direct,
                    transitive: Vec::new(),
                    tests: Vec::new(),
                    public_api: false,
                    mermaid: String::new(),
                    meta: meta_for_output.clone(),
                };

                let mut doc = ContextDocBuilder::new();
                let answer = if response_mode == ResponseMode::Full {
                    format!("impact: {} hits (fallback)", result.total_usages)
                } else {
                    format!("impact: {} hits", result.total_usages)
                };
                doc.push_answer(&answer);
                if response_mode != ResponseMode::Minimal {
                    doc.push_root_fingerprint(meta_for_output.root_fingerprint);
                }
                if response_mode == ResponseMode::Full {
                    doc.push_note("diagnostic: semantic index unavailable; using lexical fallback");
                }
                for hunk in &hunks {
                    doc.push_ref_header(&hunk.file, hunk.start_line, Some("TextMatch"));
                    doc.push_block_smart(&hunk.content);
                    doc.push_blank();
                }

                let output = CallToolResult::success(vec![Content::text(doc.finish())]);
                return Ok(attach_structured_content(
                    output,
                    &result,
                    meta_for_output,
                    "impact",
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
                            meta: ToolMeta::default(),
                        }
                    }
                }
            }
        }
    } else {
        let chunks = engine.engine_mut().context_search.hybrid().chunks();
        best_effort_text_only(symbol, chunks)
    };

    let needs_filesystem_fallback = result.total_usages == 0 && result.symbol.trim().len() >= 3;

    drop(engine);

    if needs_filesystem_fallback {
        // If the graph didn't know the symbol (common for schema-defined types or concepts
        // mentioned only in docs), fall back to a bounded filesystem grep so agents still get
        // actionable locations instead of a false "0 usages".
        let budgets = super::super::mcp_default_budgets();
        let max_hunks = 12usize;

        if let Ok(hunks) = grep_fallback_hunks(
            &root,
            &root_display,
            &result.symbol,
            response_mode,
            max_hunks,
            budgets.grep_context_max_chars,
        )
        .await
        {
            let mut seen: HashSet<(String, usize)> = result
                .direct
                .iter()
                .chain(result.transitive.iter())
                .map(|usage| (usage.file.clone(), usage.line))
                .collect();

            for hunk in hunks {
                if result.direct.len() >= MAX_DIRECT {
                    break;
                }
                let key = (hunk.file.clone(), hunk.start_line);
                if !seen.insert(key) {
                    continue;
                }
                result.direct.push(UsageInfo {
                    file: hunk.file,
                    line: hunk.start_line,
                    symbol: result.symbol.clone(),
                    relationship: "TextMatch".to_string(),
                });
            }

            result.total_usages = result.direct.len() + result.transitive.len();
            result.files_affected = count_files_affected(&result.direct, &result.transitive);
            if !result.direct.is_empty() {
                result.mermaid = ContextFinderService::generate_impact_mermaid(
                    &result.symbol,
                    &result.direct,
                    &result.transitive,
                );
            }
        }
    }
    result.meta = meta_for_output;

    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!(
        "impact: {} usages={} files={} public_api={}",
        result.symbol, result.total_usages, result.files_affected, result.public_api
    ));
    if response_mode != ResponseMode::Minimal {
        doc.push_root_fingerprint(result.meta.root_fingerprint);
    }
    if let Some(def) = result.definition.as_ref() {
        doc.push_ref_header(&def.file, def.line, Some("definition"));
    }
    if !result.direct.is_empty() {
        doc.push_note(&format!("direct: {}", result.direct.len()));
        for usage in &result.direct {
            doc.push_ref_header(&usage.file, usage.line, Some(&usage.relationship));
        }
    }
    if !result.transitive.is_empty() {
        doc.push_note(&format!("transitive: {}", result.transitive.len()));
        for usage in &result.transitive {
            doc.push_ref_header(&usage.file, usage.line, Some(&usage.relationship));
        }
    }
    if response_mode == ResponseMode::Full && !result.mermaid.trim().is_empty() {
        doc.push_note("mermaid:");
        doc.push_block_smart(&result.mermaid);
    }
    let output = CallToolResult::success(vec![Content::text(doc.finish())]);
    Ok(attach_structured_content(
        output,
        &result,
        result.meta.clone(),
        "impact",
    ))
}
