use super::super::{
    AutoIndexPolicy, CallToolResult, Content, ContextFinderService, McpError, SearchRequest,
    SearchResult,
};

/// Semantic code search
pub(in crate::tools::dispatch) async fn search(
    service: &ContextFinderService,
    request: SearchRequest,
) -> Result<CallToolResult, McpError> {
    let limit = request.limit.unwrap_or(10).clamp(1, 50);

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
    let results = {
        let (mut engine, _meta) = match service.prepare_semantic_engine(&root, policy).await {
            Ok(engine) => engine,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {e}"
                ))]));
            }
        };
        match engine
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
        }
    };

    let formatted: Vec<SearchResult> = results
        .into_iter()
        .map(|r| {
            let chunk = r.chunk;
            SearchResult {
                file: chunk.file_path,
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                symbol: chunk.metadata.symbol_name,
                symbol_type: chunk.metadata.chunk_type.map(|ct| ct.as_str().to_string()),
                score: r.score,
                content: chunk.content,
            }
        })
        .collect();

    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&formatted).unwrap_or_default(),
    )]))
}
