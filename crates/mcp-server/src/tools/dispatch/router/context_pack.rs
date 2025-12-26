use super::super::{
    build_graph_docs, current_model_id, graph_language_key, graph_nodes_store_path,
    pack_enriched_results, prepare_context_pack_enriched, tokenize_focus_query, unix_ms,
    AutoIndexPolicy, CallToolResult, Content, ContextFinderService, ContextPackOutput,
    ContextPackRequest, GraphDocConfig, GraphNodeDoc, GraphNodeStore, GraphNodeStoreMeta, McpError,
    QueryClassifier, QueryKind, QueryType, RelatedMode, CONTEXT_PACK_VERSION, GRAPH_DOC_VERSION,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;

type ToolResult<T> = std::result::Result<T, CallToolResult>;

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.into())])
}

#[derive(Clone, Copy, Debug)]
struct ContextPackFlags(u8);

impl ContextPackFlags {
    const TRACE: u8 = 1 << 0;
    const AUTO_INDEX: u8 = 1 << 1;
    const INCLUDE_DOCS: u8 = 1 << 2;
    const PREFER_CODE: u8 = 1 << 3;

    const fn trace(self) -> bool {
        self.0 & Self::TRACE != 0
    }

    const fn auto_index(self) -> bool {
        self.0 & Self::AUTO_INDEX != 0
    }

    const fn include_docs(self) -> bool {
        self.0 & Self::INCLUDE_DOCS != 0
    }

    const fn prefer_code(self) -> bool {
        self.0 & Self::PREFER_CODE != 0
    }
}

#[derive(Clone, Debug)]
struct ContextPackInputs {
    path: Option<String>,
    limit: usize,
    max_chars: usize,
    max_related_per_primary: usize,
    flags: ContextPackFlags,
    query_type: QueryType,
    strategy: context_graph::AssemblyStrategy,
    related_mode: RelatedMode,
    candidate_limit: usize,
    query_tokens: Vec<String>,
}

fn parse_strategy(
    raw: Option<&str>,
    docs_intent: bool,
    query_type: QueryType,
) -> context_graph::AssemblyStrategy {
    match raw {
        Some("direct") => context_graph::AssemblyStrategy::Direct,
        Some("deep") => context_graph::AssemblyStrategy::Deep,
        Some(_) => context_graph::AssemblyStrategy::Extended,
        None => {
            if !docs_intent && matches!(query_type, QueryType::Identifier | QueryType::Path) {
                context_graph::AssemblyStrategy::Direct
            } else {
                context_graph::AssemblyStrategy::Extended
            }
        }
    }
}

fn parse_related_mode(
    raw: Option<&str>,
    docs_intent: bool,
    query_type: QueryType,
) -> ToolResult<RelatedMode> {
    let default = if !docs_intent && matches!(query_type, QueryType::Identifier | QueryType::Path) {
        "focus"
    } else {
        "explore"
    };
    match raw.unwrap_or(default) {
        "explore" => Ok(RelatedMode::Explore),
        "focus" => Ok(RelatedMode::Focus),
        _ => Err(tool_error(
            "Error: related_mode must be 'explore' or 'focus'",
        )),
    }
}

fn parse_inputs(request: &ContextPackRequest) -> ToolResult<ContextPackInputs> {
    if request.query.trim().is_empty() {
        return Err(tool_error("Error: Query cannot be empty"));
    }

    let limit = request.limit.unwrap_or(10).clamp(1, 50);
    let max_chars = request.max_chars.unwrap_or(20_000).max(1_000);
    let max_related_per_primary = request.max_related_per_primary.unwrap_or(3).clamp(0, 12);
    let trace = request.trace.unwrap_or(false);
    let auto_index = request.auto_index.unwrap_or(true);

    let query_type = QueryClassifier::classify(&request.query);
    let docs_intent = QueryClassifier::is_docs_intent(&request.query);
    let strategy = parse_strategy(request.strategy.as_deref(), docs_intent, query_type);

    let include_docs = request.include_docs.unwrap_or(true);
    let prefer_code = request.prefer_code.unwrap_or(!docs_intent);
    let related_mode =
        parse_related_mode(request.related_mode.as_deref(), docs_intent, query_type)?;

    let candidate_limit = if include_docs && !prefer_code {
        limit.saturating_add(100).min(300)
    } else {
        limit.saturating_add(50).min(200)
    };
    let query_tokens = tokenize_focus_query(&request.query);
    let flags = {
        let mut bits = 0u8;
        if trace {
            bits |= ContextPackFlags::TRACE;
        }
        if auto_index {
            bits |= ContextPackFlags::AUTO_INDEX;
        }
        if include_docs {
            bits |= ContextPackFlags::INCLUDE_DOCS;
        }
        if prefer_code {
            bits |= ContextPackFlags::PREFER_CODE;
        }
        ContextPackFlags(bits)
    };

    Ok(ContextPackInputs {
        path: request.path.clone(),
        limit,
        max_chars,
        max_related_per_primary,
        flags,
        query_type,
        strategy,
        related_mode,
        candidate_limit,
        query_tokens,
    })
}

fn select_language(
    raw: Option<&str>,
    engine: &mut super::super::EngineLock,
) -> context_graph::GraphLanguage {
    raw.map_or_else(
        || {
            let chunks = engine.engine_mut().context_search.hybrid().chunks();
            ContextFinderService::detect_language(chunks)
        },
        |lang| ContextFinderService::parse_language(Some(lang)),
    )
}

async fn load_or_build_graph_nodes_store(
    service: &ContextFinderService,
    root: &Path,
    language: context_graph::GraphLanguage,
    source_index_mtime_ms: u64,
    max_neighbors_per_relation: usize,
    assembler: &context_graph::ContextAssembler,
) -> ToolResult<GraphNodeStore> {
    let graph_nodes_path = graph_nodes_store_path(root);
    let language_key = graph_language_key(language).to_string();
    let template_hash = service.profile.embedding().graph_node_template_hash();

    let loaded = GraphNodeStore::load(&graph_nodes_path).await.map_or_else(
        |_| None,
        |store| {
            let meta = store.meta();
            (meta.source_index_mtime_ms == source_index_mtime_ms
                && meta.graph_language == language_key
                && meta.graph_doc_version == GRAPH_DOC_VERSION
                && meta.template_hash == template_hash)
                .then_some(store)
        },
    );

    let Some(store) = loaded else {
        let docs = build_graph_docs(
            assembler,
            GraphDocConfig {
                max_neighbors_per_relation,
            },
        );
        let docs: Vec<GraphNodeDoc> = docs
            .into_iter()
            .map(|doc| {
                let text = service
                    .profile
                    .embedding()
                    .render_graph_node_doc(&doc.doc)
                    .unwrap_or(doc.doc);
                GraphNodeDoc {
                    node_id: doc.node_id,
                    chunk_id: doc.chunk_id,
                    text,
                    doc_hash: doc.doc_hash,
                }
            })
            .collect();

        let meta = GraphNodeStoreMeta::for_current_model(
            source_index_mtime_ms,
            language_key,
            GRAPH_DOC_VERSION,
            template_hash,
        )
        .map_err(|err| tool_error(format!("graph_nodes meta error: {err}")))?;

        return GraphNodeStore::build_or_update(&graph_nodes_path, meta, docs)
            .await
            .map_err(|err| tool_error(format!("graph_nodes build error: {err}")));
    };

    Ok(store)
}

fn merge_graph_node_rrf_scores(
    enriched: &[context_search::EnrichedResult],
    hits: &[context_vector_store::GraphNodeHit],
    hit_weight: f32,
) -> HashMap<String, f32> {
    const RRF_K: f32 = 60.0;

    let mut fused: HashMap<String, f32> = HashMap::new();
    for (rank, er) in enriched.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let contrib = 1.0 / (RRF_K + (rank as f32) + 1.0);
        fused
            .entry(er.primary.id.clone())
            .and_modify(|v| *v += contrib)
            .or_insert(contrib);
    }

    for (rank, hit) in hits.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let contrib = hit_weight / (RRF_K + (rank as f32) + 1.0);
        fused
            .entry(hit.chunk_id.clone())
            .and_modify(|v| *v += contrib)
            .or_insert(contrib);
    }

    fused
}

fn append_graph_node_hits(
    service: &ContextFinderService,
    assembler: &context_graph::ContextAssembler,
    chunks: &[context_code_chunker::CodeChunk],
    chunk_lookup: &HashMap<String, usize>,
    hits: &[context_vector_store::GraphNodeHit],
    strategy: context_graph::AssemblyStrategy,
    enriched: &mut Vec<context_search::EnrichedResult>,
) {
    let mut have_primary: HashSet<String> =
        enriched.iter().map(|er| er.primary.id.clone()).collect();

    for hit in hits {
        if have_primary.contains(&hit.chunk_id) {
            continue;
        }
        let Some(&chunk_idx) = chunk_lookup.get(&hit.chunk_id) else {
            continue;
        };
        let Some(chunk) = chunks.get(chunk_idx).cloned() else {
            continue;
        };
        if service.profile.is_rejected(&chunk.file_path) {
            continue;
        }

        let mut related = Vec::new();
        let mut total_lines = chunk.line_count();
        if let Ok(assembled) = assembler.assemble_for_chunk(&hit.chunk_id, strategy) {
            total_lines = assembled.total_lines;
            related = assembled
                .related_chunks
                .into_iter()
                .map(|rc| context_search::RelatedContext {
                    chunk: rc.chunk,
                    relationship_path: rc.relationship.iter().map(|r| format!("{r:?}")).collect(),
                    distance: rc.distance,
                    relevance_score: rc.relevance_score,
                })
                .collect();
        }

        enriched.push(context_search::EnrichedResult {
            primary: context_search::SearchResult {
                chunk,
                score: 0.0,
                id: hit.chunk_id.clone(),
            },
            related,
            total_lines,
            strategy,
        });
        have_primary.insert(hit.chunk_id.clone());
    }
}

fn apply_fused_scores(
    enriched: &mut [context_search::EnrichedResult],
    fused: &HashMap<String, f32>,
) {
    let mut min_score = f32::MAX;
    let mut max_score = f32::MIN;
    for er in enriched.iter() {
        if let Some(score) = fused.get(&er.primary.id) {
            min_score = min_score.min(*score);
            max_score = max_score.max(*score);
        }
    }

    let range = (max_score - min_score).max(1e-9);
    for er in enriched.iter_mut() {
        let Some(score) = fused.get(&er.primary.id) else {
            continue;
        };
        er.primary.score = if range <= 1e-9 {
            1.0
        } else {
            (*score - min_score) / range
        };
    }
}

#[derive(Clone, Copy, Debug)]
struct GraphNodesContext<'a> {
    root: &'a Path,
    language: context_graph::GraphLanguage,
    query: &'a str,
    query_type: QueryType,
    strategy: context_graph::AssemblyStrategy,
    candidate_limit: usize,
    source_index_mtime_ms: u64,
}

async fn maybe_apply_graph_nodes(
    service: &ContextFinderService,
    ctx: GraphNodesContext<'_>,
    enriched: &mut Vec<context_search::EnrichedResult>,
    engine: &mut super::super::EngineLock,
) -> ToolResult<()> {
    let graph_nodes_cfg = service.profile.graph_nodes();
    if !graph_nodes_cfg.enabled
        || matches!(ctx.strategy, context_graph::AssemblyStrategy::Direct)
        || !matches!(ctx.query_type, QueryType::Conceptual)
    {
        return Ok(());
    }

    let engine_ref = engine.engine_mut();
    let Some(assembler) = engine_ref.context_search.assembler() else {
        return Ok(());
    };

    let chunks = engine_ref.context_search.hybrid().chunks();
    let chunk_lookup = &engine_ref.chunk_lookup;

    let store = load_or_build_graph_nodes_store(
        service,
        ctx.root,
        ctx.language,
        ctx.source_index_mtime_ms,
        graph_nodes_cfg.max_neighbors_per_relation,
        assembler,
    )
    .await?;

    let embedding_query = service
        .profile
        .embedding()
        .render_query(context_vector_store::QueryKind::Conceptual, ctx.query)
        .unwrap_or_else(|_| ctx.query.to_string());
    let hits = store
        .search_with_embedding_text(&embedding_query, graph_nodes_cfg.top_k)
        .await
        .unwrap_or_default();
    if hits.is_empty() {
        return Ok(());
    }

    let fused = merge_graph_node_rrf_scores(enriched, &hits, graph_nodes_cfg.weight);
    append_graph_node_hits(
        service,
        assembler,
        chunks,
        chunk_lookup,
        &hits,
        ctx.strategy,
        enriched,
    );
    apply_fused_scores(enriched, &fused);

    enriched.sort_by(|a, b| {
        b.primary
            .score
            .total_cmp(&a.primary.score)
            .then_with(|| a.primary.id.cmp(&b.primary.id))
    });
    enriched.truncate(ctx.candidate_limit);
    Ok(())
}

fn append_trace_debug(
    contents: &mut Vec<Content>,
    service: &ContextFinderService,
    inputs: &ContextPackInputs,
    language: context_graph::GraphLanguage,
    available_models: &[String],
) {
    let query_kind = match inputs.query_type {
        QueryType::Identifier => QueryKind::Identifier,
        QueryType::Path => QueryKind::Path,
        QueryType::Conceptual => QueryKind::Conceptual,
    };
    let desired_models: Vec<String> = service
        .profile
        .experts()
        .semantic_models(query_kind)
        .to_vec();
    let graph_nodes_cfg = service.profile.graph_nodes();

    let debug = serde_json::json!({
        "query_kind": format!("{query_kind:?}"),
        "strategy": format!("{:?}", inputs.strategy),
        "language": graph_language_key(language),
        "prefer_code": inputs.flags.prefer_code(),
        "include_docs": inputs.flags.include_docs(),
        "related_mode": inputs.related_mode.as_str(),
        "semantic_models": {
            "available": available_models,
            "desired": desired_models,
        },
        "graph_nodes": {
            "enabled": graph_nodes_cfg.enabled,
            "weight": graph_nodes_cfg.weight,
            "top_k": graph_nodes_cfg.top_k,
            "max_neighbors_per_relation": graph_nodes_cfg.max_neighbors_per_relation,
        }
    });
    contents.push(Content::text(
        serde_json::to_string_pretty(&debug).unwrap_or_default(),
    ));
}

/// Build a bounded context pack for agents (single-call context).
pub(in crate::tools::dispatch) async fn context_pack(
    service: &ContextFinderService,
    request: ContextPackRequest,
) -> Result<CallToolResult, McpError> {
    let inputs = match parse_inputs(&request) {
        Ok(parsed) => parsed,
        Err(err) => return Ok(err),
    };

    let root = match service.resolve_root(inputs.path.as_deref()).await {
        Ok((root, _)) => root,
        Err(message) => return Ok(tool_error(message)),
    };

    let policy = AutoIndexPolicy::from_request(
        Some(inputs.flags.auto_index()),
        request.auto_index_budget_ms,
    );
    let (mut engine, meta) = match service.prepare_semantic_engine(&root, policy).await {
        Ok(engine) => engine,
        Err(err) => return Ok(tool_error(format!("Error: {err}"))),
    };

    let language = select_language(request.language.as_deref(), &mut engine);
    if let Err(err) = engine.engine_mut().ensure_graph(language).await {
        return Ok(tool_error(format!("Graph build error: {err}")));
    }

    let available_models = engine.engine_mut().available_models.clone();
    let source_index_mtime_ms = unix_ms(engine.engine_mut().canonical_index_mtime);

    let mut enriched = match engine
        .engine_mut()
        .context_search
        .search_with_context(&request.query, inputs.candidate_limit, inputs.strategy)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return Ok(tool_error(format!("Search error: {e}")));
        }
    };

    let graph_nodes_ctx = GraphNodesContext {
        root: &root,
        language,
        query: &request.query,
        query_type: inputs.query_type,
        strategy: inputs.strategy,
        candidate_limit: inputs.candidate_limit,
        source_index_mtime_ms,
    };
    if let Err(err) =
        maybe_apply_graph_nodes(service, graph_nodes_ctx, &mut enriched, &mut engine).await
    {
        return Ok(err);
    }

    drop(engine);

    let enriched = prepare_context_pack_enriched(
        enriched,
        inputs.limit,
        inputs.flags.prefer_code(),
        inputs.flags.include_docs(),
    );

    let (items, budget) = pack_enriched_results(
        &service.profile,
        enriched,
        inputs.max_chars,
        inputs.max_related_per_primary,
        inputs.related_mode,
        &inputs.query_tokens,
    );
    let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    let output = ContextPackOutput {
        version: CONTEXT_PACK_VERSION,
        query: request.query,
        model_id,
        profile: service.profile.name().to_string(),
        items,
        budget,
        meta: Some(meta),
    };

    let mut contents = Vec::new();
    contents.push(Content::text(
        serde_json::to_string_pretty(&output).unwrap_or_default(),
    ));

    if inputs.flags.trace() {
        append_trace_debug(&mut contents, service, &inputs, language, &available_models);
    }

    Ok(CallToolResult::success(contents))
}

#[cfg(test)]
mod tests {
    use super::{parse_inputs, ContextPackRequest};

    #[test]
    fn candidate_limit_expands_for_docs_first() {
        let request = ContextPackRequest {
            query: "README".to_string(),
            path: None,
            limit: Some(5),
            max_chars: None,
            max_related_per_primary: None,
            prefer_code: Some(false),
            include_docs: Some(true),
            related_mode: None,
            strategy: None,
            language: None,
            auto_index: None,
            auto_index_budget_ms: None,
            trace: None,
        };
        let inputs = parse_inputs(&request)
            .unwrap_or_else(|_| panic!("parse_inputs should succeed for docs-first request"));
        assert_eq!(inputs.candidate_limit, 105);
    }

    #[test]
    fn candidate_limit_expands_for_code_first() {
        let request = ContextPackRequest {
            query: "EmbeddingCache".to_string(),
            path: None,
            limit: Some(10),
            max_chars: None,
            max_related_per_primary: None,
            prefer_code: Some(true),
            include_docs: Some(true),
            related_mode: None,
            strategy: None,
            language: None,
            auto_index: None,
            auto_index_budget_ms: None,
            trace: None,
        };
        let inputs = parse_inputs(&request)
            .unwrap_or_else(|_| panic!("parse_inputs should succeed for code-first request"));
        assert_eq!(inputs.candidate_limit, 60);
    }
}
