use super::super::super::{
    graph_language_key, graph_nodes_store_path, ContextFinderService, GraphNodeStore, QueryType,
    GRAPH_DOC_VERSION,
};
use super::ToolResult;
use std::collections::{HashMap, HashSet};
use std::path::Path;

async fn load_or_build_graph_nodes_store(
    service: &ContextFinderService,
    root: &Path,
    language: context_graph::GraphLanguage,
    source_index_mtime_ms: u64,
) -> Option<GraphNodeStore> {
    let graph_nodes_path = graph_nodes_store_path(root);
    let language_key = graph_language_key(language).to_string();
    let template_hash = service.profile.embedding().graph_node_template_hash();

    GraphNodeStore::load(&graph_nodes_path)
        .await
        .ok()
        .and_then(|store| {
            let meta = store.meta();
            (meta.source_index_mtime_ms == source_index_mtime_ms
                && meta.graph_language == language_key
                && meta.graph_doc_version == GRAPH_DOC_VERSION
                && meta.template_hash == template_hash)
                .then_some(store)
        })
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
pub(super) struct GraphNodesContext<'a> {
    pub(super) root: &'a Path,
    pub(super) language: context_graph::GraphLanguage,
    pub(super) query: &'a str,
    pub(super) query_type: QueryType,
    pub(super) strategy: context_graph::AssemblyStrategy,
    pub(super) candidate_limit: usize,
    pub(super) source_index_mtime_ms: u64,
}

pub(super) async fn maybe_apply_graph_nodes(
    service: &ContextFinderService,
    ctx: GraphNodesContext<'_>,
    enriched: &mut Vec<context_search::EnrichedResult>,
    engine: &mut super::super::super::EngineLock,
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

    let store =
        load_or_build_graph_nodes_store(service, ctx.root, ctx.language, ctx.source_index_mtime_ms)
            .await;
    let Some(store) = store else {
        service
            .maybe_warm_graph_nodes_store(ctx.root.to_path_buf(), ctx.language)
            .await;
        return Ok(());
    };

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
