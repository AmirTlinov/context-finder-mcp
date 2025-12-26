use super::super::{
    AutoIndexPolicy, CallToolResult, Content, ContextFinderService, ExplainRequest, ExplainResult,
    McpError,
};
use crate::tools::util::path_has_extension_ignore_ascii_case;
use context_graph::{CodeGraph, RelationshipType};
use petgraph::graph::NodeIndex;

type ToolResult<T> = std::result::Result<T, CallToolResult>;

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.into())])
}

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
        .map_err(|e| tool_error(format!("Graph build error: {e}")))?;

    let Some(assembler) = engine.engine_mut().context_search.assembler() else {
        return Err(tool_error(
            "Graph build error: missing assembler after build",
        ));
    };
    let graph = assembler.graph();

    let Some(node) = graph.find_node(symbol) else {
        return Err(tool_error(format!("Symbol '{symbol}' not found")));
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
    let path = request.path;
    let symbol = request.symbol;
    let language = request.language;
    let root = match service.resolve_root(path.as_deref()).await {
        Ok((root, _)) => root,
        Err(message) => return Ok(tool_error(message)),
    };
    let policy = AutoIndexPolicy::from_request(request.auto_index, request.auto_index_budget_ms);
    let (mut engine, meta) = match service.prepare_semantic_engine(&root, policy).await {
        Ok(engine) => engine,
        Err(err) => return Ok(tool_error(format!("Error: {err}"))),
    };

    let data = match compute_explain_data(&mut engine, language.as_deref(), &symbol).await {
        Ok(data) => data,
        Err(err) => return Ok(err),
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
        meta: Some(meta),
    };

    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&result).unwrap_or_default(),
    )]))
}
