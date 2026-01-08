use super::super::{
    AutoIndexPolicy, CallToolResult, Content, ContextFinderService, ExplainRequest, ExplainResult,
    McpError, ResponseMode, ToolMeta,
};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::util::path_has_extension_ignore_ascii_case;
use context_code_chunker::CodeChunk;
use context_graph::{CodeGraph, RelationshipType};
use petgraph::graph::NodeIndex;
use serde_json::Value;

type ToolResult<T> = std::result::Result<T, CallToolResult>;

use super::error::{
    attach_meta, attach_structured_content, internal_error, internal_error_with_meta,
    invalid_request, invalid_request_with_meta, meta_for_request,
};
use super::semantic_fallback::{grep_fallback_hunks, is_semantic_unavailable_error};

fn format_symbol_relations(
    graph: &CodeGraph,
    rels: &[(NodeIndex, RelationshipType)],
) -> Vec<String> {
    let mut out: Vec<String> = rels
        .iter()
        .filter_map(|(n, rel)| {
            graph.get_node(*n).and_then(|nd| {
                if nd.symbol.name == "unknown"
                    || path_has_extension_ignore_ascii_case(&nd.symbol.file_path, "md")
                {
                    return None;
                }
                Some(format!("{} ({rel:?})", nd.symbol.name))
            })
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

#[derive(Debug)]
struct ExplainData {
    dependencies: Vec<String>,
    dependents: Vec<String>,
    tests: Vec<String>,
    kind: String,
    file: String,
    line: usize,
    documentation: Option<String>,
    content: String,
}

fn is_docs_path(path: &str) -> bool {
    ["md", "mdx", "rst", "adoc", "txt", "context"]
        .iter()
        .any(|ext| path_has_extension_ignore_ascii_case(path, ext))
}

fn find_best_effort_text_match(chunks: &[CodeChunk], symbol: &str) -> Option<ExplainData> {
    if symbol.trim().is_empty() {
        return None;
    }

    fn match_offset(haystack: &str, needle: &str, prefer_case_sensitive: bool) -> Option<usize> {
        if prefer_case_sensitive {
            return haystack.find(needle);
        }

        let haystack = haystack.to_lowercase();
        let needle = needle.to_lowercase();
        haystack.find(&needle)
    }

    fn explain_from_chunk(
        chunk: &CodeChunk,
        symbol: &str,
        prefer_case_sensitive: bool,
    ) -> Option<ExplainData> {
        let offset = match_offset(&chunk.content, symbol, prefer_case_sensitive)?;
        let line_offset = chunk.content[..offset]
            .bytes()
            .filter(|b| *b == b'\n')
            .count();

        let kind = if is_docs_path(&chunk.file_path) {
            "concept".to_string()
        } else {
            "text_match".to_string()
        };

        Some(ExplainData {
            dependencies: Vec::new(),
            dependents: Vec::new(),
            tests: Vec::new(),
            kind,
            file: chunk.file_path.clone(),
            line: chunk.start_line + line_offset,
            documentation: None,
            content: chunk.content.clone(),
        })
    }

    let mut candidates: Vec<&CodeChunk> = chunks.iter().collect();
    candidates.sort_by_key(|chunk| !is_docs_path(&chunk.file_path));

    for &prefer_case_sensitive in &[true, false] {
        for chunk in &candidates {
            if let Some(hit) = explain_from_chunk(chunk, symbol, prefer_case_sensitive) {
                return Some(hit);
            }
        }
    }

    None
}

async fn compute_explain_data(
    engine: &mut super::super::EngineLock,
    language: Option<&str>,
    symbol: &str,
) -> ToolResult<ExplainData> {
    let language = language.map_or_else(
        || {
            ContextFinderService::detect_language(
                engine.engine_mut().context_search.hybrid().chunks(),
            )
        },
        |lang| ContextFinderService::parse_language(Some(lang)),
    );
    engine
        .engine_mut()
        .ensure_graph(language)
        .await
        .map_err(|e| internal_error(format!("Graph build error: {e}")))?;

    let Some(assembler) = engine.engine_mut().context_search.assembler() else {
        return Err(internal_error(
            "Graph build error: missing assembler after build",
        ));
    };
    let graph = assembler.graph();

    let Some(node) = graph.find_node(symbol) else {
        // Agent-native UX: if the symbol isn't part of the code graph, it may still be a
        // repo concept (e.g. ADR terminology). Fall back to best-effort textual recall.
        if let Some(hit) = find_best_effort_text_match(
            engine.engine_mut().context_search.hybrid().chunks(),
            symbol,
        ) {
            return Ok(hit);
        }
        return Err(invalid_request(format!("Symbol '{symbol}' not found")));
    };

    let (deps, dependents_raw) = graph.get_symbol_relations(node);
    let dependencies = format_symbol_relations(graph, &deps);
    let dependents = format_symbol_relations(graph, &dependents_raw);

    let test_nodes = graph.find_related_tests(node);
    let mut tests: Vec<String> = test_nodes
        .iter()
        .filter_map(|n| graph.get_node(*n).map(|nd| nd.symbol.name.clone()))
        .collect();
    tests.sort();
    tests.dedup();

    let node_data = graph.get_node(node);
    let (kind, file, line, documentation, content) = node_data.map_or_else(
        || (String::new(), String::new(), 0, None, String::new()),
        |nd| {
            let symbol_type = &nd.symbol.symbol_type;
            let doc = nd
                .chunk
                .as_ref()
                .and_then(|c| c.metadata.documentation.clone());
            let content = nd
                .chunk
                .as_ref()
                .map_or_else(String::new, |c| c.content.clone());
            (
                format!("{symbol_type:?}"),
                nd.symbol.file_path.clone(),
                nd.symbol.start_line,
                doc,
                content,
            )
        },
    );

    Ok(ExplainData {
        dependencies,
        dependents,
        tests,
        kind,
        file,
        line,
        documentation,
        content,
    })
}

/// Deep dive into a symbol
pub(in crate::tools::dispatch) async fn explain(
    service: &ContextFinderService,
    request: ExplainRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let path = request.path;
    let symbol = request.symbol;
    let language = request.language;
    let (root, root_display) = match service.resolve_root(path.as_deref()).await {
        Ok(value) => value,
        Err(message) => {
            let meta = if response_mode == ResponseMode::Minimal {
                ToolMeta::default()
            } else {
                meta_for_request(service, path.as_deref()).await
            };
            return Ok(invalid_request_with_meta(message, meta, None, Vec::new()));
        }
    };
    let policy = AutoIndexPolicy::from_request(request.auto_index, request.auto_index_budget_ms);
    let (mut engine, meta) = match service
        .prepare_semantic_engine_for_query(&root, policy, &symbol)
        .await
    {
        Ok(engine) => engine,
        Err(err) => {
            let message = format!("Error: {err}");
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
                let hunks = match grep_fallback_hunks(
                    &root,
                    &root_display,
                    &symbol,
                    response_mode,
                    3,
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

                let (file, line, content) = hunks.first().map_or_else(
                    || ("".to_string(), 0, "".to_string()),
                    |h| (h.file.clone(), h.start_line, h.content.clone()),
                );

                let result = ExplainResult {
                    symbol: symbol.clone(),
                    kind: "unknown".to_string(),
                    file,
                    line,
                    documentation: None,
                    dependencies: Vec::new(),
                    dependents: Vec::new(),
                    tests: Vec::new(),
                    content,
                    meta: meta_for_output.clone(),
                };

                let mut doc = ContextDocBuilder::new();
                let answer = if response_mode == ResponseMode::Full {
                    "explain: best-effort (fallback)"
                } else {
                    "explain: best-effort"
                };
                doc.push_answer(answer);
                doc.push_root_fingerprint(meta_for_output.root_fingerprint);
                if response_mode == ResponseMode::Full {
                    doc.push_note("diagnostic: semantic index unavailable; using lexical fallback");
                }
                if !result.file.is_empty() && result.line > 0 {
                    doc.push_ref_header(&result.file, result.line, Some(&symbol));
                    doc.push_block_smart(&result.content);
                } else {
                    doc.push_note("no matches found for symbol (yet)");
                }

                let output = CallToolResult::success(vec![Content::text(doc.finish())]);
                return Ok(attach_structured_content(
                    output,
                    &result,
                    meta_for_output,
                    "explain",
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

    let data = match compute_explain_data(&mut engine, language.as_deref(), &symbol).await {
        Ok(data) => data,
        Err(err) => {
            // Agent-native UX: if the symbol is not part of the graph, it may still be a repo
            // concept living only in docs (ADRs/RFCs). In that case, fall back to a bounded
            // filesystem grep (even when semantic is otherwise available).
            let not_found = err
                .structured_content
                .as_ref()
                .and_then(|v| v.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .is_some_and(|msg| msg.contains(" not found"));
            drop(engine);
            if not_found {
                let budgets = super::super::mcp_default_budgets();
                if let Ok(hunks) = grep_fallback_hunks(
                    &root,
                    &root_display,
                    &symbol,
                    response_mode,
                    3,
                    budgets.grep_context_max_chars,
                )
                .await
                {
                    if let Some(first) = hunks.first() {
                        let kind = if is_docs_path(&first.file) {
                            "concept"
                        } else {
                            "text_match"
                        };
                        let result = ExplainResult {
                            symbol: symbol.clone(),
                            kind: kind.to_string(),
                            file: first.file.clone(),
                            line: first.start_line,
                            documentation: None,
                            dependencies: Vec::new(),
                            dependents: Vec::new(),
                            tests: Vec::new(),
                            content: first.content.clone(),
                            meta: meta_for_output.clone(),
                        };

                        let mut doc = ContextDocBuilder::new();
                        doc.push_answer("explain: best-effort (filesystem fallback)");
                        doc.push_root_fingerprint(meta_for_output.root_fingerprint);
                        if response_mode != ResponseMode::Minimal {
                            doc.push_note("fallback: grep_context (symbol not in graph)");
                        }
                        doc.push_ref_header(&result.file, result.line, Some(&result.symbol));
                        doc.push_block_smart(&result.content);
                        let output = CallToolResult::success(vec![Content::text(doc.finish())]);
                        return Ok(attach_structured_content(
                            output,
                            &result,
                            meta_for_output,
                            "explain",
                        ));
                    }
                }
            }

            return Ok(if response_mode == ResponseMode::Minimal {
                err
            } else {
                attach_meta(err, meta_for_output.clone())
            });
        }
    };
    drop(engine);

    let result = ExplainResult {
        symbol,
        kind: data.kind,
        file: data.file,
        line: data.line,
        documentation: data.documentation,
        dependencies: data.dependencies,
        dependents: data.dependents,
        tests: data.tests,
        content: data.content,
        meta: meta_for_output,
    };

    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!(
        "explain: {} {}:{}",
        result.symbol, result.file, result.line
    ));
    doc.push_root_fingerprint(result.meta.root_fingerprint);
    doc.push_ref_header(&result.file, result.line, Some(result.symbol.as_str()));
    doc.push_block_smart(&result.content);
    if response_mode == ResponseMode::Full {
        if let Some(text) = result.documentation.as_deref() {
            if !text.trim().is_empty() {
                doc.push_blank();
                doc.push_note("documentation:");
                doc.push_block_smart(text);
            }
        }
        if !result.dependencies.is_empty() {
            doc.push_blank();
            doc.push_note(&format!("dependencies: {}", result.dependencies.len()));
            for item in &result.dependencies {
                doc.push_line(&format!("- {item}"));
            }
        }
        if !result.dependents.is_empty() {
            doc.push_blank();
            doc.push_note(&format!("dependents: {}", result.dependents.len()));
            for item in &result.dependents {
                doc.push_line(&format!("- {item}"));
            }
        }
        if !result.tests.is_empty() {
            doc.push_blank();
            doc.push_note(&format!("tests: {}", result.tests.len()));
            for item in &result.tests {
                doc.push_line(&format!("- {item}"));
            }
        }
    }
    let output = CallToolResult::success(vec![Content::text(doc.finish())]);
    Ok(attach_structured_content(
        output,
        &result,
        result.meta.clone(),
        "explain",
    ))
}
