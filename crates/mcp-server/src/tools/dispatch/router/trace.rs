use super::super::{
    AutoIndexPolicy, CallToolResult, Content, ContextFinderService, McpError, ResponseMode,
    ToolMeta, TraceRequest, TraceResult, TraceStep,
};
use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_request_with_meta,
    meta_for_request,
};
use super::semantic_fallback::{grep_fallback_hunks, is_semantic_unavailable_error};
use crate::tools::context_doc::ContextDocBuilder;

/// Trace call path between two symbols
pub(in crate::tools::dispatch) async fn trace(
    service: &ContextFinderService,
    request: TraceRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let (root, root_display) = match service.resolve_root(request.path.as_deref()).await {
        Ok((root, root_display)) => (root, root_display),
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
    let query = format!("{} {}", request.from, request.to);
    let (mut engine, meta) = match service
        .prepare_semantic_engine_for_query(&root, policy, &query)
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
                let max_hunks = 4usize;

                let from_hunks = match grep_fallback_hunks(
                    &root,
                    &root_display,
                    &request.from,
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

                let to_hunks = match grep_fallback_hunks(
                    &root,
                    &root_display,
                    &request.to,
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

                let result = TraceResult {
                    found: false,
                    path: Vec::new(),
                    depth: 0,
                    mermaid: String::new(),
                    meta: meta_for_output.clone(),
                };

                let mut doc = ContextDocBuilder::new();
                let answer = if response_mode == ResponseMode::Full {
                    "trace: best-effort (fallback)"
                } else {
                    "trace: best-effort"
                };
                doc.push_answer(answer);
                if response_mode != ResponseMode::Minimal {
                    doc.push_root_fingerprint(meta_for_output.root_fingerprint);
                }
                if response_mode == ResponseMode::Full {
                    doc.push_note("diagnostic: semantic index unavailable; using lexical anchors");
                }
                if !from_hunks.is_empty() {
                    doc.push_note(&format!("from: {}", request.from));
                    for hunk in &from_hunks {
                        doc.push_ref_header(
                            &hunk.file,
                            hunk.start_line,
                            Some(request.from.as_str()),
                        );
                        doc.push_block_smart(&hunk.content);
                        doc.push_blank();
                    }
                }
                if !to_hunks.is_empty() {
                    doc.push_note(&format!("to: {}", request.to));
                    for hunk in &to_hunks {
                        doc.push_ref_header(&hunk.file, hunk.start_line, Some(request.to.as_str()));
                        doc.push_block_smart(&hunk.content);
                        doc.push_blank();
                    }
                }

                let output = CallToolResult::success(vec![Content::text(doc.finish())]);
                return Ok(attach_structured_content(
                    output,
                    &result,
                    meta_for_output,
                    "trace",
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

    let language = request.language.as_deref().map_or_else(
        || {
            ContextFinderService::detect_language(
                engine.engine_mut().context_search.hybrid().chunks(),
            )
        },
        |lang| ContextFinderService::parse_language(Some(lang)),
    );

    if let Err(e) = engine.engine_mut().ensure_graph(language).await {
        return Ok(internal_error_with_meta(
            format!("Graph build error: {e}"),
            meta_for_output.clone(),
        ));
    }

    let (found, path_steps, depth) = {
        let Some(assembler) = engine.engine_mut().context_search.assembler() else {
            return Ok(internal_error_with_meta(
                "Graph build error: missing assembler after build",
                meta_for_output.clone(),
            ));
        };
        let graph = assembler.graph();

        // Find both symbols
        let Some(from_node) = graph.find_node(&request.from) else {
            return Ok(invalid_request_with_meta(
                format!("Symbol '{}' not found", request.from),
                meta_for_output.clone(),
                None,
                Vec::new(),
            ));
        };

        let Some(to_node) = graph.find_node(&request.to) else {
            return Ok(invalid_request_with_meta(
                format!("Symbol '{}' not found", request.to),
                meta_for_output.clone(),
                None,
                Vec::new(),
            ));
        };

        // Find path
        let path_with_edges = graph.find_path_with_edges(from_node, to_node);

        path_with_edges.map_or_else(
            || (false, Vec::new(), 0),
            |path| {
                let steps: Vec<TraceStep> = path
                    .iter()
                    .map(|(n, rel)| {
                        let node_data = graph.get_node(*n);
                        let (symbol, file, line) = node_data.map_or_else(
                            || (String::new(), String::new(), 0),
                            |nd| {
                                (
                                    nd.symbol.name.clone(),
                                    nd.symbol.file_path.clone(),
                                    nd.symbol.start_line,
                                )
                            },
                        );
                        TraceStep {
                            symbol,
                            file,
                            line,
                            relationship: rel.map(|r| format!("{r:?}")),
                        }
                    })
                    .collect();
                let depth = steps.len().saturating_sub(1);
                (true, steps, depth)
            },
        )
    };

    drop(engine);

    // Generate Mermaid sequence diagram
    let mermaid = ContextFinderService::generate_trace_mermaid(&path_steps);

    let result = TraceResult {
        found,
        path: path_steps,
        depth,
        mermaid,
        meta: meta_for_output,
    };

    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!(
        "trace: found={} depth={}",
        result.found, result.depth
    ));
    if response_mode != ResponseMode::Minimal {
        doc.push_root_fingerprint(result.meta.root_fingerprint);
    }
    for step in &result.path {
        doc.push_ref_header(&step.file, step.line, Some(step.symbol.as_str()));
        if let Some(rel) = step.relationship.as_deref() {
            doc.push_note(&format!("relationship: {rel}"));
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
        "trace",
    ))
}
