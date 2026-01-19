use super::super::{
    current_model_id, graph_language_key, graph_nodes_store_path, pack_enriched_results,
    prepare_context_pack_enriched, tokenize_focus_query, unix_ms, AutoIndexPolicy, CallToolResult,
    Content, ContextFinderService, ContextPackBudget, ContextPackItem, ContextPackOutput,
    ContextPackRequest, GraphNodeStore, McpError, QueryClassifier, QueryKind, QueryType,
    RelatedMode, ResponseMode, CONTEXT_PACK_VERSION, GRAPH_DOC_VERSION,
};
use crate::tools::context_doc::ContextDocBuilder;
use context_protocol::{enforce_max_chars, BudgetTruncation, ToolNextAction};
use std::collections::{HashMap, HashSet};
use std::path::Path;

type ToolResult<T> = std::result::Result<T, CallToolResult>;

use super::error::{
    attach_meta, internal_error_with_meta, invalid_request, invalid_request_with_meta,
    meta_for_request,
};
use super::semantic_fallback::{grep_fallback_hunks, is_semantic_unavailable_error};

#[derive(Clone, Copy, Debug)]
struct ContextPackFlags(u8);

impl ContextPackFlags {
    const TRACE: u8 = 1 << 0;
    const INCLUDE_DOCS: u8 = 1 << 1;
    const PREFER_CODE: u8 = 1 << 2;

    const fn trace(self) -> bool {
        self.0 & Self::TRACE != 0
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
    include_paths: Vec<String>,
    exclude_paths: Vec<String>,
    file_pattern: Option<String>,
    flags: ContextPackFlags,
    response_mode: ResponseMode,
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
        _ => Err(invalid_request(
            "Error: related_mode must be 'explore' or 'focus'",
        )),
    }
}

fn choose_fallback_token(tokens: &[String]) -> Option<String> {
    fn is_low_value(token_lc: &str) -> bool {
        matches!(
            token_lc,
            "struct"
                | "definition"
                | "define"
                | "defined"
                | "fn"
                | "function"
                | "method"
                | "class"
                | "type"
                | "enum"
                | "trait"
                | "impl"
                | "module"
                | "file"
                | "path"
                | "usage"
                | "usages"
                | "reference"
                | "references"
                | "where"
                | "find"
                | "show"
        )
    }

    let mut best: Option<String> = None;
    for token in tokens {
        let token = token.trim();
        if token.len() < 4 {
            continue;
        }
        let token_lc = token.to_lowercase();
        if is_low_value(&token_lc) {
            continue;
        }
        let looks_like_identifier = token
            .chars()
            .any(|ch| ch.is_ascii_uppercase() || ch == '_' || ch == '-');
        if !looks_like_identifier && token.len() < 8 {
            continue;
        }
        if best.as_ref().is_none_or(|b| token.len() > b.len()) {
            best = Some(token.to_string());
        }
    }

    best.or_else(|| {
        tokens
            .iter()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .max_by_key(|t| t.len())
            .map(|t| t.to_string())
    })
}

fn items_mention_token(items: &[ContextPackItem], token: &str) -> bool {
    let token = token.trim();
    if token.is_empty() {
        return true;
    }
    let token_lc = token.to_lowercase();
    items.iter().take(6).any(|item| {
        item.symbol
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(token))
            || item.file.contains(token)
            || item.file.to_lowercase().contains(&token_lc)
            || item.content.contains(token)
            || item.content.to_lowercase().contains(&token_lc)
    })
}

fn enforce_context_pack_budget(output: &mut ContextPackOutput) -> ToolResult<()> {
    let max_chars = output.budget.max_chars;
    let res = enforce_max_chars(
        output,
        max_chars,
        |inner, used| inner.budget.used_chars = used,
        |inner| {
            inner.budget.truncated = true;
            if inner.budget.truncation.is_none() {
                inner.budget.truncation = Some(BudgetTruncation::MaxChars);
            }
        },
        |inner| {
            if inner.items.len() > 1 {
                inner.items.pop();
                inner.budget.dropped_items += 1;
                return true;
            }

            let Some(item) = inner.items.last_mut() else {
                return false;
            };

            // Keep at least one anchor item. Shrink content before giving up.
            if !item.imports.is_empty() {
                item.imports.clear();
                return true;
            }

            if !item.content.is_empty() {
                let target = item.content.len().div_ceil(2);
                let mut cut = target.min(item.content.len());
                while cut > 0 && !item.content.is_char_boundary(cut) {
                    cut = cut.saturating_sub(1);
                }
                if cut == 0 {
                    item.content.clear();
                } else {
                    item.content.truncate(cut);
                }
                return true;
            }

            if item.relationship.is_some() {
                item.relationship = None;
                return true;
            }
            if item.distance.is_some() {
                item.distance = None;
                return true;
            }
            if item.chunk_type.is_some() {
                item.chunk_type = None;
                return true;
            }
            if item.symbol.is_some() {
                item.symbol = None;
                return true;
            }

            false
        },
    );
    match res {
        Ok(_) => Ok(()),
        Err(_err) => {
            // Fail-soft: under extremely small budgets the envelope can dominate even a
            // single-item pack. Prefer returning an empty (but valid) pack over erroring.
            output.items.clear();
            output.budget.truncated = true;
            if output.budget.truncation.is_none() {
                output.budget.truncation = Some(BudgetTruncation::MaxChars);
            }
            Ok(())
        }
    }
}

fn parse_inputs(request: &ContextPackRequest) -> ToolResult<ContextPackInputs> {
    if request.query.trim().is_empty() {
        return Err(invalid_request("Error: Query cannot be empty"));
    }

    let limit = request.limit.unwrap_or(10).clamp(1, 50);
    let max_chars = request.max_chars.unwrap_or(2_000).max(1_000);
    let max_related_per_primary = request.max_related_per_primary.unwrap_or(3).clamp(0, 12);
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let trace = request.trace.unwrap_or(false) && response_mode == ResponseMode::Full;

    let query_type = QueryClassifier::classify(&request.query);
    let docs_intent = QueryClassifier::is_docs_intent(&request.query);
    let strategy = parse_strategy(request.strategy.as_deref(), docs_intent, query_type);

    let include_docs = request.include_docs.unwrap_or(true);
    let prefer_code = request.prefer_code.unwrap_or(!docs_intent);
    let related_mode =
        parse_related_mode(request.related_mode.as_deref(), docs_intent, query_type)?;

    let include_paths = request.include_paths.clone().unwrap_or_default();
    let exclude_paths = request.exclude_paths.clone().unwrap_or_default();
    let file_pattern = request
        .file_pattern
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(ToString::to_string);

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
        include_paths,
        exclude_paths,
        file_pattern,
        flags,
        response_mode,
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
        context_protocol::serialize_json(&debug).unwrap_or_default(),
    ));
}

struct LexicalFallbackArgs<'a> {
    query: &'a str,
    fallback_pattern: &'a str,
    meta: context_indexer::ToolMeta,
    reason_note: Option<&'a str>,
}

async fn build_lexical_fallback_result(
    service: &ContextFinderService,
    root: &Path,
    root_display: &str,
    inputs: &ContextPackInputs,
    mut args: LexicalFallbackArgs<'_>,
) -> ToolResult<CallToolResult> {
    let budgets = super::super::mcp_default_budgets();
    let fallback_max_chars = inputs.max_chars.min(budgets.grep_context_max_chars);
    let max_hunks = inputs.limit.min(10);

    let hunks = grep_fallback_hunks(
        root,
        root_display,
        args.fallback_pattern,
        inputs.response_mode,
        max_hunks,
        fallback_max_chars,
    )
    .await
    .map_err(|err| invalid_request(format!("Error: lexical fallback grep failed ({err:#})")))?;

    let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    let profile = service.profile.name().to_string();

    let mut items = Vec::new();
    let mut used_chars = 0usize;
    let mut dropped_items = 0usize;

    for (idx, hunk) in hunks.into_iter().enumerate() {
        if items.len() >= inputs.limit {
            dropped_items += 1;
            continue;
        }

        let content_chars = hunk.content.chars().count();
        if used_chars.saturating_add(content_chars) > inputs.max_chars {
            dropped_items += 1;
            continue;
        }
        used_chars = used_chars.saturating_add(content_chars);

        items.push(ContextPackItem {
            id: format!("lexical:{}:{}:{}", hunk.file, hunk.start_line, idx),
            role: "primary".to_string(),
            file: hunk.file,
            start_line: hunk.start_line,
            end_line: hunk.end_line,
            symbol: None,
            chunk_type: None,
            score: (1.0 - idx as f32 * 0.01).max(0.0),
            imports: Vec::new(),
            content: hunk.content,
            relationship: None,
            distance: None,
        });
    }

    let truncated = dropped_items > 0;
    let budget = ContextPackBudget {
        max_chars: inputs.max_chars,
        used_chars,
        truncated,
        dropped_items,
        truncation: truncated.then_some(BudgetTruncation::MaxChars),
    };

    let mut next_actions = Vec::new();
    if inputs.response_mode == ResponseMode::Full {
        next_actions.push(ToolNextAction {
            tool: "text_search".to_string(),
            args: serde_json::json!({
                "path": root_display,
                "pattern": args.fallback_pattern,
                "max_results": 80,
                "case_sensitive": false,
                "whole_word": true,
                "response_mode": "facts"
            }),
            reason: "Verify the exact anchor term via text_search (helps detect wrong root, typos, or stale index).".to_string(),
        });
        next_actions.push(ToolNextAction {
            tool: "repo_onboarding_pack".to_string(),
            args: serde_json::json!({
                "path": root_display,
                "max_chars": 12000,
                "response_mode": "facts"
            }),
            reason: "If results still look wrong, re-onboard the repo to confirm the effective root and key docs.".to_string(),
        });
    }

    if inputs.response_mode == ResponseMode::Minimal {
        args.meta.index_state = None;
    }

    let mut output = ContextPackOutput {
        version: CONTEXT_PACK_VERSION,
        query: args.query.to_string(),
        model_id,
        profile,
        items,
        budget,
        next_actions,
        meta: args.meta,
    };

    enforce_context_pack_budget(&mut output)?;

    let mut doc = ContextDocBuilder::new();
    let answer = if inputs.response_mode == ResponseMode::Full {
        format!("context_pack: {} items (fallback)", output.items.len())
    } else {
        format!("context_pack: {} items", output.items.len())
    };
    doc.push_answer(&answer);
    doc.push_root_fingerprint(output.meta.root_fingerprint);
    if inputs.response_mode == ResponseMode::Full {
        if let Some(note) = args.reason_note {
            doc.push_note(note);
        }
        doc.push_note(&format!("fallback_pattern: {}", args.fallback_pattern));
    }
    if output.items.is_empty() && inputs.response_mode != ResponseMode::Minimal {
        if inputs.response_mode == ResponseMode::Full {
            doc.push_note("no matches found for fallback pattern");
        } else {
            doc.push_note("no matches found");
        }
    }
    for item in &output.items {
        doc.push_ref_header(&item.file, item.start_line, item.symbol.as_deref());
        doc.push_block_smart(&item.content);
        doc.push_blank();
    }

    let (rendered, envelope_truncated) = doc.finish_bounded(output.budget.max_chars);
    if envelope_truncated {
        output.budget.truncated = true;
        if output.budget.truncation.is_none() {
            output.budget.truncation = Some(BudgetTruncation::MaxChars);
        }
    }
    let mut result = CallToolResult::success(vec![Content::text(rendered)]);
    let structured = serde_json::to_value(&output).map_err(|err| {
        internal_error_with_meta(
            format!("Error: failed to serialize context_pack output ({err})"),
            output.meta.clone(),
        )
    })?;
    result.structured_content = Some(structured);
    Ok(result)
}

/// Build a bounded context pack for agents (single-call context).
pub(in crate::tools::dispatch) async fn context_pack(
    service: &ContextFinderService,
    request: ContextPackRequest,
) -> Result<CallToolResult, McpError> {
    let inputs = match parse_inputs(&request) {
        Ok(parsed) => parsed,
        Err(err) => {
            let meta = meta_for_request(service, request.path.as_deref()).await;
            return Ok(attach_meta(err, meta));
        }
    };

    let mut hints: Vec<String> = Vec::new();
    if !inputs.include_paths.is_empty() {
        hints.extend(inputs.include_paths.iter().cloned());
    }
    if !inputs.exclude_paths.is_empty() {
        hints.extend(inputs.exclude_paths.iter().cloned());
    }
    if let Some(pattern) = inputs.file_pattern.as_deref() {
        if !pattern.trim().is_empty() {
            hints.push(pattern.to_string());
        }
    }
    let (root, root_display) = match service
        .resolve_root_with_hints(inputs.path.as_deref(), &hints)
        .await
    {
        Ok(value) => value,
        Err(message) => {
            let meta = meta_for_request(service, inputs.path.as_deref()).await;
            return Ok(invalid_request_with_meta(message, meta, None, Vec::new()));
        }
    };

    let policy = AutoIndexPolicy::from_request(request.auto_index, request.auto_index_budget_ms);
    let (mut engine, meta) = match service
        .prepare_semantic_engine_for_query(&root, policy, &request.query)
        .await
    {
        Ok(engine) => engine,
        Err(err) => {
            let message = format!("Error: {err}");
            let meta = service.tool_meta(&root).await;
            if is_semantic_unavailable_error(&message) {
                let fallback_pattern = inputs
                    .query_tokens
                    .iter()
                    .max_by_key(|t| t.len())
                    .cloned()
                    .unwrap_or_else(|| request.query.trim().to_string());
                match build_lexical_fallback_result(
                    service,
                    &root,
                    &root_display,
                    &inputs,
                    LexicalFallbackArgs {
                        query: &request.query,
                        fallback_pattern: &fallback_pattern,
                        meta,
                        reason_note: Some(
                            "diagnostic: semantic index unavailable; using lexical fallback",
                        ),
                    },
                )
                .await
                {
                    Ok(result) => return Ok(result),
                    Err(err) => return Ok(attach_meta(err, service.tool_meta(&root).await)),
                }
            }

            return Ok(internal_error_with_meta(message, meta));
        }
    };

    let language = select_language(request.language.as_deref(), &mut engine);
    if let Err(err) = engine.engine_mut().ensure_graph(language).await {
        return Ok(internal_error_with_meta(
            format!("Graph build error: {err}"),
            meta.clone(),
        ));
    }

    let available_models = engine.engine_mut().loaded_model_ids();
    let source_index_mtime_ms = unix_ms(engine.engine_mut().canonical_index_mtime);

    let mut enriched = match engine
        .engine_mut()
        .context_search
        .search_with_context(&request.query, inputs.candidate_limit, inputs.strategy)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return Ok(internal_error_with_meta(
                format!("Search error: {e}"),
                meta.clone(),
            ));
        }
    };
    let semantic_disabled_reason = engine
        .engine_mut()
        .context_search
        .hybrid()
        .semantic_disabled_reason()
        .map(str::to_string);

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
        return Ok(attach_meta(err, meta.clone()));
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
        &inputs.include_paths,
        &inputs.exclude_paths,
        inputs.file_pattern.as_deref(),
        inputs.related_mode,
        &inputs.query_tokens,
    );
    let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    let query = request.query.clone();
    let mut output = ContextPackOutput {
        version: CONTEXT_PACK_VERSION,
        query: query.clone(),
        model_id,
        profile: service.profile.name().to_string(),
        items,
        budget,
        next_actions: Vec::new(),
        meta,
    };

    // Guardrail: if a query contains a strong anchor (identifier/path), never return a pack that
    // doesn't mention it. This prevents "high-confidence junk" in mixed queries like
    // "LintWarning struct definition" when the identifier is missing from the repo.
    if !output.items.is_empty()
        && matches!(inputs.query_type, QueryType::Identifier | QueryType::Path)
        && !QueryClassifier::is_docs_intent(&request.query)
    {
        if let Some(anchor) = choose_fallback_token(&inputs.query_tokens) {
            if !items_mention_token(&output.items, &anchor) {
                match build_lexical_fallback_result(
                    service,
                    &root,
                    &root_display,
                    &inputs,
                    LexicalFallbackArgs {
                        query: &request.query,
                        fallback_pattern: &anchor,
                        meta: output.meta.clone(),
                        reason_note: Some("semantic: anchor_missing (fallback to filesystem)"),
                    },
                )
                .await
                {
                    Ok(result) => return Ok(result),
                    Err(err) => {
                        return Ok(attach_meta(
                            err,
                            meta_for_request(service, inputs.path.as_deref()).await,
                        ))
                    }
                }
            }
        }
    }

    match inputs.response_mode {
        ResponseMode::Minimal => {
            output.meta.index_state = None;
        }
        ResponseMode::Facts => {}
        ResponseMode::Full => {
            if output.items.is_empty() && semantic_disabled_reason.is_some() {
                let budgets = super::super::mcp_default_budgets();
                let pattern = inputs
                    .query_tokens
                    .iter()
                    .max_by_key(|t| t.len())
                    .cloned()
                    .unwrap_or_else(|| query.trim().to_string());
                output.next_actions.push(ToolNextAction {
                    tool: "rg".to_string(),
                    args: serde_json::json!({
                        "path": root_display.clone(),
                        "pattern": pattern,
                        "literal": true,
                        "case_sensitive": false,
                        "context": 2,
                        "max_chars": budgets.grep_context_max_chars,
                        "max_hunks": 8,
                        "format": "numbered",
                        "response_mode": "facts"
                    }),
                    reason: "Semantic search is disabled; fall back to rg on the most relevant query token.".to_string(),
                });
            }

            if output.items.is_empty() {
                let pattern = choose_fallback_token(&inputs.query_tokens)
                    .or_else(|| inputs.query_tokens.iter().max_by_key(|t| t.len()).cloned())
                    .unwrap_or_else(|| request.query.trim().to_string());

                output.next_actions.push(ToolNextAction {
                    tool: "text_search".to_string(),
                    args: serde_json::json!({
                        "path": root_display.clone(),
                        "pattern": pattern,
                        "max_results": 80,
                        "case_sensitive": false,
                        "whole_word": true,
                        "response_mode": "facts"
                    }),
                    reason: "No semantic hits; verify the strongest query anchor via text_search (detects typos, wrong root, or stale index).".to_string(),
                });

                output.next_actions.push(ToolNextAction {
                    tool: "repo_onboarding_pack".to_string(),
                    args: serde_json::json!({
                        "path": root_display.clone(),
                        "max_chars": 12000,
                        "response_mode": "facts"
                    }),
                    reason: "No semantic hits; re-onboard the repo to confirm the effective root and key docs.".to_string(),
                });
            } else if let Some(token) = choose_fallback_token(&inputs.query_tokens) {
                if !items_mention_token(&output.items, &token) {
                    output.next_actions.push(ToolNextAction {
                        tool: "text_search".to_string(),
                        args: serde_json::json!({
                            "path": root_display.clone(),
                            "pattern": token,
                            "max_results": 80,
                            "case_sensitive": false,
                            "whole_word": true,
                            "response_mode": "facts"
                        }),
                        reason: "Semantic hits do not mention the key query token; verify the exact term via text_search (often reveals wrong root or stale index).".to_string(),
                    });
                }
            }

            let next_max_chars = output.budget.max_chars.saturating_mul(2).min(500_000);
            let retry_action = {
                let mut args = serde_json::json!({
                    "path": root_display.clone(),
                    "query": query,
                    "max_chars": next_max_chars,
                });
                if let Some(obj) = args.as_object_mut() {
                    if !inputs.include_paths.is_empty() {
                        obj.insert(
                            "include_paths".to_string(),
                            serde_json::to_value(&inputs.include_paths).unwrap_or_default(),
                        );
                    }
                    if !inputs.exclude_paths.is_empty() {
                        obj.insert(
                            "exclude_paths".to_string(),
                            serde_json::to_value(&inputs.exclude_paths).unwrap_or_default(),
                        );
                    }
                    if let Some(pattern) = inputs.file_pattern.as_deref() {
                        obj.insert(
                            "file_pattern".to_string(),
                            serde_json::Value::String(pattern.to_string()),
                        );
                    }
                }

                ToolNextAction {
                    tool: "context_pack".to_string(),
                    args,
                    reason: "Retry context_pack with a larger max_chars budget.".to_string(),
                }
            };
            if output.budget.truncated {
                output.next_actions.push(retry_action.clone());
            }
            if let Err(result) = enforce_context_pack_budget(&mut output) {
                return Ok(result);
            }
            if output.budget.truncated && output.next_actions.is_empty() {
                output.next_actions.push(retry_action);
                if let Err(result) = enforce_context_pack_budget(&mut output) {
                    return Ok(result);
                }
            }

            let mut doc = ContextDocBuilder::new();
            doc.push_answer(&format!("context_pack: {} items", output.items.len()));
            doc.push_root_fingerprint(output.meta.root_fingerprint);
            if output.items.is_empty() && inputs.response_mode != ResponseMode::Minimal {
                doc.push_note("no matches found");
            }
            if let Some(reason) = semantic_disabled_reason.as_deref() {
                if inputs.response_mode == ResponseMode::Full {
                    doc.push_note("semantic: disabled (embeddings unavailable; using fuzzy-only).");
                    doc.push_note(&format!("semantic_error: {reason}"));
                    if output.items.is_empty() {
                        doc.push_note(
                            "next: grep_context (semantic disabled; fallback to literal grep)",
                        );
                    }
                }
            }
            for (idx, item) in output.items.iter().enumerate() {
                let mut meta_parts = Vec::new();
                meta_parts.push(format!("role={}", item.role));
                meta_parts.push(format!("score={:.3}", item.score));
                if let Some(kind) = item.chunk_type.as_deref() {
                    meta_parts.push(format!("type={kind}"));
                }
                if let Some(distance) = item.distance {
                    meta_parts.push(format!("distance={distance}"));
                }
                if let Some(rel) = item.relationship.as_ref().filter(|r| !r.is_empty()) {
                    meta_parts.push(format!("rel={}", rel.join("->")));
                }
                if !item.imports.is_empty() {
                    meta_parts.push(format!("imports={}", item.imports.len()));
                }
                doc.push_note(&format!("hit {}: {}", idx + 1, meta_parts.join(" ")));
                doc.push_ref_header(&item.file, item.start_line, item.symbol.as_deref());
                doc.push_block_smart(&item.content);
                doc.push_blank();
            }
            if output.budget.truncated {
                doc.push_note("truncated=true (increase max_chars)");
            }

            let mut contents = vec![Content::text(doc.finish())];
            if inputs.flags.trace() {
                append_trace_debug(&mut contents, service, &inputs, language, &available_models);
            }

            let mut result = CallToolResult::success(contents);
            match serde_json::to_value(&output) {
                Ok(structured) => {
                    result.structured_content = Some(structured);
                }
                Err(err) => {
                    return Ok(internal_error_with_meta(
                        format!("Error: failed to serialize context_pack output ({err})"),
                        output.meta.clone(),
                    ));
                }
            }
            return Ok(result);
        }
    }
    if let Err(result) = enforce_context_pack_budget(&mut output) {
        return Ok(result);
    }

    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!("context_pack: {} items", output.items.len()));
    doc.push_root_fingerprint(output.meta.root_fingerprint);
    if output.items.is_empty() && inputs.response_mode != ResponseMode::Minimal {
        doc.push_note("no matches found");
    }
    if inputs.response_mode == ResponseMode::Full {
        if let Some(reason) = semantic_disabled_reason.as_deref() {
            doc.push_note("semantic: disabled (embeddings unavailable; using fuzzy-only).");
            doc.push_note(&format!("semantic_error: {reason}"));
        }
    }
    for item in &output.items {
        doc.push_ref_header(&item.file, item.start_line, item.symbol.as_deref());
        doc.push_block_smart(&item.content);
        doc.push_blank();
    }
    if output.budget.truncated {
        doc.push_note("truncated=true (increase max_chars)");
    }
    let mut result = CallToolResult::success(vec![Content::text(doc.finish())]);
    match serde_json::to_value(&output) {
        Ok(structured) => {
            result.structured_content = Some(structured);
        }
        Err(err) => {
            return Ok(internal_error_with_meta(
                format!("Error: failed to serialize context_pack output ({err})"),
                output.meta.clone(),
            ));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{enforce_context_pack_budget, parse_inputs, ContextPackRequest};
    use context_indexer::ToolMeta;
    use context_search::{ContextPackBudget, ContextPackItem, ContextPackOutput};

    #[test]
    fn candidate_limit_expands_for_docs_first() {
        let request = ContextPackRequest {
            query: "README".to_string(),
            path: None,
            limit: Some(5),
            max_chars: None,
            include_paths: None,
            exclude_paths: None,
            file_pattern: None,
            max_related_per_primary: None,
            prefer_code: Some(false),
            include_docs: Some(true),
            related_mode: None,
            strategy: None,
            language: None,
            response_mode: None,
            trace: None,
            auto_index: None,
            auto_index_budget_ms: None,
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
            include_paths: None,
            exclude_paths: None,
            file_pattern: None,
            max_related_per_primary: None,
            prefer_code: Some(true),
            include_docs: Some(true),
            related_mode: None,
            strategy: None,
            language: None,
            response_mode: None,
            trace: None,
            auto_index: None,
            auto_index_budget_ms: None,
        };
        let inputs = parse_inputs(&request)
            .unwrap_or_else(|_| panic!("parse_inputs should succeed for code-first request"));
        assert_eq!(inputs.candidate_limit, 60);
    }

    #[test]
    fn enforce_budget_shrinks_last_item_instead_of_dropping_to_zero() {
        let mut output = ContextPackOutput {
            version: 1,
            query: "q".to_string(),
            model_id: "m".to_string(),
            profile: "p".to_string(),
            items: vec![ContextPackItem {
                id: "id".to_string(),
                role: "primary".to_string(),
                file: "src/lib.rs".to_string(),
                start_line: 1,
                end_line: 1,
                symbol: Some("alpha".to_string()),
                chunk_type: Some("Function".to_string()),
                score: 1.0,
                imports: vec!["std::fmt".to_string()],
                content: "x".repeat(10_000),
                relationship: None,
                distance: None,
            }],
            budget: ContextPackBudget {
                max_chars: 1_000,
                used_chars: 0,
                truncated: false,
                dropped_items: 0,
                truncation: None,
            },
            next_actions: Vec::new(),
            meta: ToolMeta::default(),
        };

        let result = enforce_context_pack_budget(&mut output);
        assert!(result.is_ok(), "expected budget enforcement to succeed");
        assert_eq!(output.items.len(), 1, "expected an anchor item to remain");
        assert!(
            output.budget.truncated,
            "expected truncation under tight max_chars"
        );
        assert!(
            output.items[0].content.len() < 10_000,
            "expected item content to be shrunk"
        );
    }
}
