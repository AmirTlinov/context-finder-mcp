use super::super::{
    AutoIndexPolicy, CallToolResult, Content, ContextFinderService, ContextHit, ContextRequest,
    ContextResult, McpError, RelatedCode,
};

/// Search with graph context
pub(in crate::tools::dispatch) async fn context(
    service: &ContextFinderService,
    request: ContextRequest,
) -> Result<CallToolResult, McpError> {
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

    let enriched = {
        let language = request.language.as_deref().map_or_else(
            || {
                ContextFinderService::detect_language(
                    engine.engine_mut().context_search.hybrid().chunks(),
                )
            },
            |lang| ContextFinderService::parse_language(Some(lang)),
        );

        if let Err(e) = engine.engine_mut().ensure_graph(language).await {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Graph build error: {e}"
            ))]));
        }

        match engine
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
        }
    };

    drop(engine);

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

            let symbol = er.primary.chunk.metadata.symbol_name;
            ContextHit {
                file: er.primary.chunk.file_path,
                start_line: er.primary.chunk.start_line,
                end_line: er.primary.chunk.end_line,
                symbol,
                score: er.primary.score,
                content: er.primary.chunk.content,
                related,
            }
        })
        .collect();

    let result = ContextResult {
        results,
        related_count,
        meta: Some(meta),
    };

    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&result).unwrap_or_default(),
    )]))
}
