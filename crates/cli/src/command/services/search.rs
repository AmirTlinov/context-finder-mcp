use crate::command::context::{
    ensure_index_exists, graph_nodes_path, index_path, index_path_for_model, load_store_mtime,
    unix_ms, CommandContext,
};
use crate::command::domain::{
    config_bool_path, config_string_path, config_usize_path, parse_payload, CommandOutcome,
    ContextPackBudget, ContextPackItem, ContextPackOutput, ContextPackPayload, Hint, HintKind,
    NextAction, NextActionKind, RelatedCodeOutput, SearchOutput, SearchPayload, SearchResultOutput,
    SearchStrategy, SearchWithContextPayload, TaskPackItem, TaskPackOutput, TaskPackPayload,
    TASK_PACK_VERSION,
};
use crate::command::infra::{GraphCacheFactory, HealthPort};
use crate::command::warm;
use anyhow::{anyhow, Context as AnyhowContext, Result};
use context_graph::{
    build_graph_docs, ContextAssembler, GraphDocConfig, GraphLanguage, GRAPH_DOC_VERSION,
};
use context_protocol::{enforce_max_chars, finalize_used_chars, BudgetTruncation, ToolNextAction};
use context_search::{EnrichedResult, RelatedContext};
use context_search::{
    MultiModelContextSearch, MultiModelHybridSearch, QueryClassifier, QueryType, SearchProfile,
    CONTEXT_PACK_VERSION,
};
use context_vector_store::{
    classify_path_kind, corpus_path_for_project_root, current_model_id, ChunkCorpus, DocumentKind,
    GraphNodeDoc, GraphNodeStore, GraphNodeStoreMeta, QueryKind, SearchResult, VectorIndex,
};
use itertools::Itertools;
use log::{debug, warn};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::{Instant, SystemTime};

pub struct SearchService {
    graph: GraphCacheFactory,
    health: HealthPort,
}

fn join_limited(items: &[String], max: usize) -> String {
    if items.is_empty() {
        return "[]".to_string();
    }
    if items.len() <= max {
        return items.join(", ");
    }
    format!(
        "{} …(+{})",
        items[..max].join(", "),
        items.len().saturating_sub(max)
    )
}

impl SearchService {
    pub fn new(
        graph: GraphCacheFactory,
        health: HealthPort,
        _cache: crate::command::infra::CompareCacheAdapter,
    ) -> Self {
        Self { graph, health }
    }

    pub async fn basic(&self, payload: Value, ctx: &CommandContext) -> Result<CommandOutcome> {
        let payload: SearchPayload = parse_payload(payload)?;
        if payload.query.trim().is_empty() {
            return Err(anyhow!("Query must not be empty"));
        }
        let project_ctx = ctx.resolve_project(payload.project).await?;
        let _ = crate::heartbeat::ping(&project_ctx.root).await;
        let warm = warm::global_warmer().prewarm(&project_ctx.root).await;
        let (strategy_hint, _reason_hint) = choose_task_hint(&payload.query);
        let limit = payload
            .limit
            .or_else(|| config_usize_path(&project_ctx.config, &["defaults", "search", "limit"]))
            .unwrap_or(crate::command::domain::DEFAULT_LIMIT);
        let trace = payload
            .trace
            .or_else(|| config_bool_path(&project_ctx.config, &["defaults", "search", "trace"]))
            .unwrap_or(false);
        let load_index_start = Instant::now();
        let loaded = load_semantic_indexes(&project_ctx.root, &project_ctx.profile)
            .await
            .context("Failed to load semantic indices")?;
        let timing_load_index_ms = load_index_start.elapsed().as_millis() as u64;
        let store_path = loaded.store_path;
        let store_mtime = loaded.store_mtime;
        let index_size_bytes = loaded.index_size_bytes;

        let sources = loaded.sources;
        let profile = project_ctx.profile.clone();
        let corpus = load_chunk_corpus(&project_ctx.root).await?;
        let mut search = if let Some(corpus) = corpus {
            MultiModelHybridSearch::from_env_with_corpus(sources, profile, corpus)
        } else {
            MultiModelHybridSearch::from_env(sources, profile)
        }
        .context("Failed to create search engine")?;
        let search_start = Instant::now();
        let results = search
            .search(&payload.query, limit)
            .await
            .context("Search failed")?;
        let timing_search_ms = search_start.elapsed().as_millis() as u64;

        let mut formatted: Vec<_> = results.into_iter().map(format_basic_output).collect();
        annotate_reasons(&payload.query, &mut formatted);
        let (deduped, dropped) = dedup_results(formatted, &project_ctx.profile);

        if trace {
            trace_results(&payload.query, &deduped);
        }

        let mut outcome = CommandOutcome::from_value(SearchOutput {
            query: payload.query.clone(),
            results: deduped,
        })?;
        outcome.meta.config_path = project_ctx.config_path;
        outcome.meta.profile = Some(project_ctx.profile_name.clone());
        outcome.meta.profile_path = project_ctx.profile_path.clone();
        outcome.meta.index_updated = Some(false);
        outcome.meta.index_mtime_ms = Some(unix_ms(store_mtime));
        outcome.meta.index_size_bytes = index_size_bytes;
        outcome.meta.timing_load_index_ms = Some(timing_load_index_ms);
        outcome.meta.timing_search_ms = Some(timing_search_ms);
        outcome.meta.warm = Some(warm.warmed);
        outcome.meta.warm_cost_ms = Some(warm.warm_cost_ms);
        outcome.meta.warm_graph_cache_hit = Some(warm.graph_cache_hit);
        let (task_hint, reason_hint) = choose_task_hint(&payload.query);
        if let Some(h) = strategy_hint {
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: h,
            });
        }
        if let Some(h) = task_hint {
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: h,
            });
        }
        if let Some(h) = reason_hint {
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: h,
            });
        }
        if dropped > 0 {
            outcome.meta.duplicates_dropped = Some(dropped);
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: format!("Deduplicated {dropped} overlapping results"),
            });
        }
        outcome.hints.extend(project_ctx.hints.into_iter());
        outcome.hints.push(Hint {
            kind: HintKind::Cache,
            text: format!(
                "Reusing existing index at {} (mtime {} ms)",
                store_path.display(),
                unix_ms(store_mtime)
            ),
        });
        self.health.attach(&project_ctx.root, &mut outcome).await;
        Ok(outcome)
    }

    pub async fn with_context(
        &self,
        payload: Value,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome> {
        let payload: SearchWithContextPayload = parse_payload(payload)?;
        if payload.query.trim().is_empty() {
            return Err(anyhow!("Query must not be empty"));
        }
        if payload.show_graph.unwrap_or(false) && payload.strategy == Some(SearchStrategy::Direct) {
            return Err(anyhow!(
                "Graph output requires context depth >= 1 (use extended/deep strategy)"
            ));
        }
        let project_ctx = ctx.resolve_project(payload.project).await?;
        let warm = warm::global_warmer().prewarm(&project_ctx.root).await;
        let (task_hint, reason_hint) = choose_task_hint(&payload.query);
        let limit = payload
            .limit
            .or_else(|| {
                config_usize_path(
                    &project_ctx.config,
                    &["defaults", "search_with_context", "limit"],
                )
            })
            .unwrap_or(crate::command::domain::DEFAULT_LIMIT);
        let (strategy, strategy_hint) = match payload.strategy {
            Some(s) => (s, None),
            None => {
                if let Some(cfg) = config_string_path(
                    &project_ctx.config,
                    &["defaults", "search_with_context", "strategy"],
                )
                .and_then(|value| SearchStrategy::from_name(&value))
                {
                    (cfg, None)
                } else {
                    choose_strategy(&payload.query)
                }
            }
        };
        let show_graph = payload
            .show_graph
            .or_else(|| {
                config_bool_path(
                    &project_ctx.config,
                    &["defaults", "search_with_context", "show_graph"],
                )
            })
            .unwrap_or(false);
        let trace = payload
            .trace
            .or_else(|| {
                config_bool_path(
                    &project_ctx.config,
                    &["defaults", "search_with_context", "trace"],
                )
            })
            .unwrap_or(false);
        let reuse_graph = payload
            .reuse_graph
            .or_else(|| {
                config_bool_path(
                    &project_ctx.config,
                    &["defaults", "search_with_context", "reuse_graph"],
                )
            })
            .unwrap_or(true);

        let load_index_start = Instant::now();
        let loaded = load_semantic_indexes(&project_ctx.root, &project_ctx.profile)
            .await
            .context("Failed to load semantic indices")?;
        let timing_load_index_ms = load_index_start.elapsed().as_millis() as u64;
        let store_path = loaded.store_path;
        let store_mtime = loaded.store_mtime;
        let index_size_bytes = loaded.index_size_bytes;

        let language_pref = payload.language.clone().or_else(|| {
            config_string_path(
                &project_ctx.config,
                &["defaults", "search_with_context", "language"],
            )
            .or_else(|| crate::command::context::graph_language_from_config(&project_ctx.config))
        });
        let language = language_pref
            .as_deref()
            .map(parse_graph_language)
            .transpose()?
            .unwrap_or(GraphLanguage::Rust);

        let graph_cache = self.graph.for_root(&project_ctx.root);
        let mut graph_cache_used = false;

        let graph_start = Instant::now();

        let sources = loaded.sources;
        let mut available_semantic_models: Vec<String> =
            sources.iter().map(|(id, _)| id.clone()).collect();
        available_semantic_models.sort();
        let profile = project_ctx.profile.clone();
        let corpus = load_chunk_corpus(&project_ctx.root).await?;
        let hybrid = if let Some(corpus) = corpus {
            MultiModelHybridSearch::from_env_with_corpus(sources, profile, corpus)
        } else {
            MultiModelHybridSearch::from_env(sources, profile)
        }
        .context("Failed to create search engine")?;
        let chunk_lookup = build_chunk_lookup(hybrid.chunks());

        let cached_assembler = if reuse_graph {
            graph_cache
                .load(store_mtime, language, hybrid.chunks(), &chunk_lookup)
                .await?
        } else {
            None
        };

        let mut context_search =
            MultiModelContextSearch::new(hybrid).context("Failed to create context search")?;

        if let Some(assembler) = cached_assembler {
            context_search.set_assembler(assembler);
            graph_cache_used = true;
        }

        if context_search.assembler().is_none() {
            context_search
                .build_graph(language)
                .context("Failed to build code graph")?;
            if reuse_graph {
                if let Some(assembler) = context_search.assembler() {
                    if let Err(err) = graph_cache.save(store_mtime, language, assembler).await {
                        warn!("Failed to store graph cache: {err}");
                    }
                }
            }
        }
        let timing_graph_ms = graph_start.elapsed().as_millis() as u64;

        let search_start = Instant::now();
        let enriched_results = context_search
            .search_with_context(&payload.query, limit, strategy.to_assembly())
            .await
            .context("Context search failed")?;
        let timing_search_ms = search_start.elapsed().as_millis() as u64;

        let mut formatted: Vec<_> = enriched_results
            .into_iter()
            .map(|er| format_enriched_output(er, show_graph, &project_ctx.profile))
            .collect();
        annotate_reasons(&payload.query, &mut formatted);
        let (results, dropped) = dedup_results(formatted, &project_ctx.profile);

        let output = SearchOutput {
            query: payload.query.clone(),
            results: results.clone(),
        };

        if trace {
            trace_results(&payload.query, &results);
        }

        let mut outcome = CommandOutcome::from_value(output)?;
        outcome.meta.graph_cache = Some(graph_cache_used);
        if graph_cache_used {
            outcome.hints.push(Hint {
                kind: HintKind::Cache,
                text: "Graph cache hit (reused assembler)".to_string(),
            });
        }
        outcome.meta.config_path = project_ctx.config_path;
        outcome.meta.profile = Some(project_ctx.profile_name.clone());
        outcome.meta.profile_path = project_ctx.profile_path.clone();
        outcome.meta.index_updated = Some(false);
        outcome.meta.index_mtime_ms = Some(unix_ms(store_mtime));
        outcome.meta.index_size_bytes = index_size_bytes;
        outcome.meta.warm = Some(warm.warmed);
        outcome.meta.warm_cost_ms = Some(warm.warm_cost_ms);
        outcome.meta.warm_graph_cache_hit = Some(warm.graph_cache_hit);
        outcome.meta.timing_load_index_ms = Some(timing_load_index_ms);
        outcome.meta.timing_graph_ms = Some(timing_graph_ms);
        outcome.meta.timing_search_ms = Some(timing_search_ms);
        if let Some(hint) = strategy_hint {
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: hint,
            });
        }
        if let Some(hint) = task_hint {
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: hint,
            });
        }
        if let Some(hint) = reason_hint {
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: hint,
            });
        }
        if let Some(fps) = outcome.meta.health_files_per_sec {
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: format!(
                    "Indexer throughput: {:.1} files/s, p95 {} ms",
                    fps,
                    outcome.meta.health_p95_ms.unwrap_or(0)
                ),
            });
        }
        const MAX_PATHS: usize = 5;
        const MAX_SEGMENTS: usize = 4;
        let related_paths: Vec<String> = results
            .iter()
            .filter_map(|r| r.related.as_ref())
            .flatten()
            .take(MAX_PATHS)
            .map(|rel| {
                let path = rel
                    .graph_path
                    .as_ref()
                    .map(|p| truncate_path(p, MAX_SEGMENTS))
                    .unwrap_or_else(|| truncate_path(&rel.relationship.join(" -> "), MAX_SEGMENTS));
                let edge_types = if rel.relationship.is_empty() {
                    "edge".to_string()
                } else {
                    rel.relationship
                        .iter()
                        .take(MAX_SEGMENTS)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("/")
                };
                format!("{}: {} [{}]", rel.file, path, edge_types)
            })
            .collect();
        if !related_paths.is_empty() {
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: format!("graph paths: {}", related_paths.join("; ")),
            });
        }
        if dropped > 0 {
            outcome.meta.duplicates_dropped = Some(dropped);
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: format!("Deduplicated {dropped} overlapping results"),
            });
        }
        if let Some((nodes, edges)) = context_search.graph_stats() {
            outcome.meta.graph_nodes = Some(nodes);
            outcome.meta.graph_edges = Some(edges);
        }
        outcome.meta.graph_cache_size_bytes = graph_cache.size_bytes().await;
        outcome.hints.extend(project_ctx.hints.into_iter());
        outcome.hints.push(Hint {
            kind: HintKind::Cache,
            text: format!(
                "Index ready at {} (mtime {} ms)",
                store_path.display(),
                unix_ms(store_mtime)
            ),
        });

        if graph_cache_used {
            if let Some((nodes, edges)) = context_search.graph_stats() {
                outcome.hints.push(Hint {
                    kind: HintKind::Cache,
                    text: format!(
                        "Graph cache hit ({:?}) — {} nodes / {} edges reused",
                        language, nodes, edges
                    ),
                });
            }
        } else if reuse_graph {
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: format!(
                    "Graph rebuilt for language {:?}; future runs will reuse cache unless reuse_graph=false",
                    language
                ),
            });
        } else {
            outcome.hints.push(Hint {
                kind: HintKind::Warn,
                text: "Graph caching disabled for this request (reuse_graph=false)".to_string(),
            });
        }

        self.health.attach(&project_ctx.root, &mut outcome).await;
        Ok(outcome)
    }

    pub async fn context_pack(
        &self,
        payload: Value,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome> {
        let payload: ContextPackPayload = parse_payload(payload)?;
        if payload.query.trim().is_empty() {
            return Err(anyhow!("Query must not be empty"));
        }

        let project_ctx = ctx.resolve_project(payload.project).await?;
        let request_options = ctx.request_options();
        let warm = warm::global_warmer().prewarm(&project_ctx.root).await;

        let limit = payload
            .limit
            .or_else(|| {
                config_usize_path(&project_ctx.config, &["defaults", "context_pack", "limit"])
            })
            .unwrap_or(crate::command::domain::DEFAULT_LIMIT);

        let max_chars = payload
            .max_chars
            .or_else(|| {
                config_usize_path(
                    &project_ctx.config,
                    &["defaults", "context_pack", "max_chars"],
                )
            })
            .unwrap_or(20_000);

        let max_related_per_primary = payload
            .max_related_per_primary
            .or_else(|| {
                config_usize_path(
                    &project_ctx.config,
                    &["defaults", "context_pack", "max_related_per_primary"],
                )
            })
            .unwrap_or(3)
            .min(12);

        let trace = payload
            .trace
            .or_else(|| {
                config_bool_path(&project_ctx.config, &["defaults", "context_pack", "trace"])
            })
            .unwrap_or(false);
        let reuse_graph = payload
            .reuse_graph
            .or_else(|| {
                config_bool_path(
                    &project_ctx.config,
                    &["defaults", "context_pack", "reuse_graph"],
                )
            })
            .unwrap_or(true);

        let (strategy, strategy_hint) = match payload.strategy {
            Some(s) => (s, None),
            None => {
                if let Some(cfg) = config_string_path(
                    &project_ctx.config,
                    &["defaults", "context_pack", "strategy"],
                )
                .and_then(|value| SearchStrategy::from_name(&value))
                {
                    (cfg, None)
                } else {
                    choose_strategy(&payload.query)
                }
            }
        };

        let query_type = QueryClassifier::classify(&payload.query);
        let docs_intent = QueryClassifier::is_docs_intent(&payload.query);
        let include_docs = payload.include_docs.unwrap_or(true);
        let prefer_code = payload.prefer_code.unwrap_or(!docs_intent);
        let related_mode =
            parse_related_mode(payload.related_mode.as_deref(), docs_intent, query_type)?;
        let query_tokens = tokenize_focus_query(&payload.query);

        let load_index_start = Instant::now();
        let loaded = load_semantic_indexes(&project_ctx.root, &project_ctx.profile)
            .await
            .context("Failed to load semantic indices")?;
        let timing_load_index_ms = load_index_start.elapsed().as_millis() as u64;
        let _store_path = loaded.store_path;
        let store_mtime = loaded.store_mtime;
        let index_size_bytes = loaded.index_size_bytes;

        let language_pref = payload.language.clone().or_else(|| {
            config_string_path(
                &project_ctx.config,
                &["defaults", "context_pack", "language"],
            )
            .or_else(|| crate::command::context::graph_language_from_config(&project_ctx.config))
        });
        let language = language_pref
            .as_deref()
            .map(parse_graph_language)
            .transpose()?
            .unwrap_or(GraphLanguage::Rust);

        let graph_cache = self.graph.for_root(&project_ctx.root);
        let mut graph_cache_used = false;

        let graph_start = Instant::now();

        let sources = loaded.sources;
        let mut available_semantic_models: Vec<String> =
            sources.iter().map(|(id, _)| id.clone()).collect();
        available_semantic_models.sort();
        let profile = project_ctx.profile.clone();
        let corpus = load_chunk_corpus(&project_ctx.root).await?;
        let hybrid = if let Some(corpus) = corpus {
            MultiModelHybridSearch::from_env_with_corpus(sources, profile, corpus)
        } else {
            MultiModelHybridSearch::from_env(sources, profile)
        }
        .context("Failed to create search engine")?;
        let chunk_lookup = build_chunk_lookup(hybrid.chunks());

        let cached_assembler = if reuse_graph {
            graph_cache
                .load(store_mtime, language, hybrid.chunks(), &chunk_lookup)
                .await?
        } else {
            None
        };

        let mut context_search =
            MultiModelContextSearch::new(hybrid).context("Failed to create context search")?;

        if let Some(assembler) = cached_assembler {
            context_search.set_assembler(assembler);
            graph_cache_used = true;
        }

        if context_search.assembler().is_none() {
            context_search
                .build_graph(language)
                .context("Failed to build code graph")?;
            if reuse_graph {
                if let Some(assembler) = context_search.assembler() {
                    if let Err(err) = graph_cache.save(store_mtime, language, assembler).await {
                        warn!("Failed to store graph cache: {err}");
                    }
                }
            }
        }
        let timing_graph_ms = graph_start.elapsed().as_millis() as u64;

        let assembly_strategy = strategy.to_assembly();
        let candidate_limit = if include_docs && !prefer_code {
            limit.saturating_add(100).min(300)
        } else {
            limit.saturating_add(50).min(200)
        };
        let search_start = Instant::now();
        let mut enriched_results = context_search
            .search_with_context(&payload.query, candidate_limit, assembly_strategy)
            .await
            .context("Context search failed")?;
        let timing_search_ms = search_start.elapsed().as_millis() as u64;

        let graph_nodes_cfg = project_ctx.profile.graph_nodes();
        let mut graph_nodes_hint: Option<String> = None;

        if graph_nodes_cfg.enabled
            && strategy != SearchStrategy::Direct
            && matches!(query_type, QueryType::Conceptual)
        {
            if let Some(assembler) = context_search.assembler() {
                match load_or_build_graph_nodes_store(
                    &project_ctx.root,
                    unix_ms(store_mtime),
                    language,
                    assembler,
                    graph_nodes_cfg.max_neighbors_per_relation,
                    project_ctx.profile.embedding(),
                )
                .await
                {
                    Ok((graph_nodes_store, cache_hit)) => {
                        let embedding_query = project_ctx
                            .profile
                            .embedding()
                            .render_query(QueryKind::Conceptual, &payload.query)
                            .unwrap_or_else(|_| payload.query.clone());
                        let hits = graph_nodes_store
                            .search_with_embedding_text(&embedding_query, graph_nodes_cfg.top_k)
                            .await
                            .unwrap_or_default();

                        graph_nodes_hint = Some(format!(
                            "graph_nodes: {} (hits={})",
                            if cache_hit { "cache_hit" } else { "rebuilt" },
                            hits.len()
                        ));

                        if !hits.is_empty() {
                            // Weighted RRF fusion of base ranking + graph_nodes ranking.
                            const RRF_K: f32 = 60.0;
                            let mut fused: HashMap<String, f32> = HashMap::new();

                            for (rank, er) in enriched_results.iter().enumerate() {
                                #[allow(clippy::cast_precision_loss)]
                                let contrib = 1.0 / (RRF_K + (rank as f32) + 1.0);
                                fused
                                    .entry(er.primary.id.clone())
                                    .and_modify(|v| *v += contrib)
                                    .or_insert(contrib);
                            }

                            for (rank, hit) in hits.iter().enumerate() {
                                #[allow(clippy::cast_precision_loss)]
                                let contrib =
                                    graph_nodes_cfg.weight / (RRF_K + (rank as f32) + 1.0);
                                fused
                                    .entry(hit.chunk_id.clone())
                                    .and_modify(|v| *v += contrib)
                                    .or_insert(contrib);
                            }

                            let mut have_primary: HashSet<String> = enriched_results
                                .iter()
                                .map(|er| er.primary.id.clone())
                                .collect();

                            // Build enriched entries for any graph_nodes-only candidates.
                            for hit in hits {
                                if have_primary.contains(&hit.chunk_id) {
                                    continue;
                                }
                                let Some(&chunk_idx) = chunk_lookup.get(&hit.chunk_id) else {
                                    continue;
                                };
                                let Some(chunk) =
                                    context_search.hybrid().chunks().get(chunk_idx).cloned()
                                else {
                                    continue;
                                };
                                if project_ctx.profile.is_rejected(&chunk.file_path) {
                                    continue;
                                }

                                let mut related = Vec::new();
                                let mut total_lines = chunk.line_count();
                                if let Ok(assembled) =
                                    assembler.assemble_for_chunk(&hit.chunk_id, assembly_strategy)
                                {
                                    total_lines = assembled.total_lines;
                                    related = assembled
                                        .related_chunks
                                        .into_iter()
                                        .map(|rc| context_search::RelatedContext {
                                            chunk: rc.chunk,
                                            relationship_path: rc
                                                .relationship
                                                .iter()
                                                .map(|r| format!("{r:?}"))
                                                .collect(),
                                            distance: rc.distance,
                                            relevance_score: rc.relevance_score,
                                        })
                                        .collect();
                                }

                                enriched_results.push(EnrichedResult {
                                    primary: SearchResult {
                                        chunk,
                                        // Will be replaced by fused normalization below.
                                        score: 0.0,
                                        id: hit.chunk_id.clone(),
                                    },
                                    related,
                                    total_lines,
                                    strategy: assembly_strategy,
                                });
                                have_primary.insert(hit.chunk_id);
                            }

                            // Normalize fused scores to 0..1 and sort deterministically.
                            let mut min_score = f32::MAX;
                            let mut max_score = f32::MIN;
                            for er in &enriched_results {
                                if let Some(score) = fused.get(&er.primary.id) {
                                    min_score = min_score.min(*score);
                                    max_score = max_score.max(*score);
                                }
                            }
                            let range = (max_score - min_score).max(1e-9);

                            for er in &mut enriched_results {
                                if let Some(score) = fused.get(&er.primary.id) {
                                    er.primary.score = if range <= 1e-9 {
                                        1.0
                                    } else {
                                        (*score - min_score) / range
                                    };
                                }
                            }

                            enriched_results.sort_by(|a, b| {
                                b.primary
                                    .score
                                    .total_cmp(&a.primary.score)
                                    .then_with(|| {
                                        a.primary.chunk.file_path.cmp(&b.primary.chunk.file_path)
                                    })
                                    .then_with(|| {
                                        a.primary.chunk.start_line.cmp(&b.primary.chunk.start_line)
                                    })
                            });
                            enriched_results.truncate(candidate_limit);
                        }
                    }
                    Err(err) => warn!("graph_nodes disabled: {err:#}"),
                }
            }
        }

        let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());

        let enriched_results =
            prepare_context_pack_enriched(enriched_results, limit, prefer_code, include_docs);

        let (items, budget, filtered_out) = pack_enriched_results(
            enriched_results,
            &project_ctx.profile,
            max_chars,
            max_related_per_primary,
            &request_options,
            related_mode,
            &query_tokens,
        );

        let query = payload.query.clone();
        let project_root = project_ctx.root.display().to_string();
        let mut output = ContextPackOutput {
            version: CONTEXT_PACK_VERSION,
            query: query.clone(),
            model_id,
            profile: project_ctx.profile_name.clone(),
            items,
            budget,
            next_actions: Vec::new(),
            meta: context_indexer::ToolMeta {
                index_state: None,
                root_fingerprint: Some(context_indexer::root_fingerprint(&project_root)),
            },
        };
        enforce_context_pack_budget(&mut output)?;

        let debug_hints = if trace {
            let query_kind = match query_type {
                QueryType::Identifier => QueryKind::Identifier,
                QueryType::Path => QueryKind::Path,
                QueryType::Conceptual => QueryKind::Conceptual,
            };
            let desired_models: Vec<String> = project_ctx
                .profile
                .experts()
                .semantic_models(query_kind)
                .to_vec();

            let available_set: HashSet<&str> = available_semantic_models
                .iter()
                .map(String::as_str)
                .collect();
            let mut selected_models: Vec<String> = desired_models
                .iter()
                .filter(|id| available_set.contains(id.as_str()))
                .cloned()
                .collect();
            if selected_models.is_empty() {
                if let Some(first) = available_semantic_models.first().cloned() {
                    selected_models.push(first);
                }
            }

            vec![
                Hint {
                    kind: HintKind::Info,
                    text: format!(
                        "debug: query_kind={query_kind:?} strategy={strategy:?} language={}",
                        language_pref.as_deref().unwrap_or("rust")
                    ),
                },
                Hint {
                    kind: HintKind::Info,
                    text: format!(
                        "debug: prefer_code={prefer_code} include_docs={include_docs} related_mode={}",
                        related_mode.as_str()
                    ),
                },
                Hint {
                    kind: HintKind::Info,
                    text: format!(
                        "debug: semantic_models available=[{}] desired=[{}] selected=[{}]",
                        join_limited(&available_semantic_models, 8),
                        join_limited(&desired_models, 8),
                        join_limited(&selected_models, 8)
                    ),
                },
                Hint {
                    kind: HintKind::Info,
                    text: format!(
                        "debug: pack items={} chars={}/{} truncated={} dropped={}",
                        output.items.len(),
                        output.budget.used_chars,
                        output.budget.max_chars,
                        output.budget.truncated,
                        output.budget.dropped_items
                    ),
                },
            ]
        } else {
            Vec::new()
        };

        if trace {
            for item in &output.items {
                debug!(
                    "pack item: {} {}:{}-{} ({})",
                    item.role, item.file, item.start_line, item.end_line, item.score
                );
            }
        }

        let budget_truncated = output.budget.truncated;
        let next_max_chars = output.budget.max_chars.saturating_mul(2).min(500_000);
        let retry_action = ToolNextAction {
            tool: "context_pack".to_string(),
            args: serde_json::json!({
                "project": project_root,
                "query": query,
                "max_chars": next_max_chars
            }),
            reason: "Retry context_pack with a larger max_chars budget.".to_string(),
        };
        if budget_truncated {
            output.next_actions.push(retry_action.clone());
        }
        let mut outcome = CommandOutcome::from_value(output)?;
        outcome.hints.extend(debug_hints);
        outcome.meta.graph_cache = Some(graph_cache_used);
        outcome.meta.config_path = project_ctx.config_path;
        outcome.meta.profile = Some(project_ctx.profile_name.clone());
        outcome.meta.profile_path = project_ctx.profile_path.clone();
        outcome.meta.index_updated = Some(false);
        outcome.meta.index_mtime_ms = Some(unix_ms(store_mtime));
        outcome.meta.index_size_bytes = index_size_bytes;
        outcome.meta.warm = Some(warm.warmed);
        outcome.meta.warm_cost_ms = Some(warm.warm_cost_ms);
        outcome.meta.warm_graph_cache_hit = Some(warm.graph_cache_hit);
        outcome.meta.timing_load_index_ms = Some(timing_load_index_ms);
        outcome.meta.timing_graph_ms = Some(timing_graph_ms);
        outcome.meta.timing_search_ms = Some(timing_search_ms);
        if let Some(hint) = strategy_hint {
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: hint,
            });
        }
        if let Some(hint) = graph_nodes_hint {
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: hint,
            });
        }
        if crate::command::path_filters::is_active(&request_options) && filtered_out > 0 {
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: format!("Path filters excluded {filtered_out} pack items"),
            });
        }
        if budget_truncated {
            outcome.next_actions.push(retry_action);
        }
        self.health.attach(&project_ctx.root, &mut outcome).await;
        Ok(outcome)
    }

    pub async fn task_pack(&self, payload: Value, ctx: &CommandContext) -> Result<CommandOutcome> {
        let payload: TaskPackPayload = parse_payload(payload)?;
        if payload.intent.trim().is_empty() {
            return Err(anyhow!("Intent must not be empty"));
        }

        let ctx_payload = ContextPackPayload {
            query: payload.intent.clone(),
            limit: payload.limit,
            project: payload.project,
            strategy: payload.strategy,
            max_chars: payload.max_chars,
            max_related_per_primary: payload.max_related_per_primary,
            prefer_code: payload.prefer_code,
            include_docs: payload.include_docs,
            related_mode: payload.related_mode,
            trace: payload.trace,
            language: payload.language,
            reuse_graph: payload.reuse_graph,
        };

        let mut outcome = self
            .context_pack(serde_json::to_value(ctx_payload)?, ctx)
            .await?;

        let pack: ContextPackOutput = serde_json::from_value(outcome.data.clone())
            .context("Invalid context_pack output (expected ContextPackOutput)")?;

        let task_pack = build_task_pack(&payload.intent, pack);
        outcome.data = serde_json::to_value(task_pack)?;
        Ok(outcome)
    }
}

fn build_task_pack(intent: &str, pack: ContextPackOutput) -> TaskPackOutput {
    let mut primary_files = Vec::new();
    let mut seen = HashSet::new();
    let mut primary = 0usize;
    let mut related = 0usize;

    let items: Vec<TaskPackItem> = pack
        .items
        .into_iter()
        .map(|item| {
            match item.role.as_str() {
                "primary" => primary += 1,
                _ => related += 1,
            }
            if item.role == "primary" && seen.insert(item.file.clone()) {
                primary_files.push(item.file.clone());
            }
            TaskPackItem {
                why: explain_pack_item(&item),
                item,
            }
        })
        .collect();

    let digest = {
        let files = primary_files.iter().take(3).cloned().collect::<Vec<_>>();
        let files_hint = if files.is_empty() {
            String::new()
        } else {
            format!(" Top files: {}.", files.join(", "))
        };
        Some(format!(
            "Intent: {}. Pack: {} primary / {} related.{}",
            intent.trim(),
            primary,
            related,
            files_hint
        ))
    };

    let mut next_actions = Vec::new();
    for file in primary_files.into_iter().take(3) {
        next_actions.push(NextAction {
            kind: NextActionKind::OpenFile,
            reason: "Inspect primary context".to_string(),
            file: Some(file),
            command: None,
            query: None,
        });
    }

    TaskPackOutput {
        version: TASK_PACK_VERSION,
        intent: intent.to_string(),
        model_id: pack.model_id,
        profile: pack.profile,
        digest,
        items,
        next_actions,
        budget: pack.budget,
    }
}

fn explain_pack_item(item: &ContextPackItem) -> Vec<String> {
    let mut why = Vec::new();
    if item.role == "primary" {
        why.push("Primary match".to_string());
    } else {
        why.push("Related context".to_string());
    }
    if let Some(symbol) = &item.symbol {
        if !symbol.is_empty() {
            why.push(format!("Symbol: {symbol}"));
        }
    }
    if let Some(rel) = &item.relationship {
        if !rel.is_empty() {
            why.push(format!("Relationship: {}", rel.join(" / ")));
        }
    }
    if let Some(distance) = item.distance {
        why.push(format!("Graph distance: {distance}"));
    }
    why
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RelatedMode {
    Explore,
    Focus,
}

impl RelatedMode {
    #[must_use]
    const fn as_str(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::Focus => "focus",
        }
    }
}

fn parse_related_mode(
    raw: Option<&str>,
    docs_intent: bool,
    query_type: QueryType,
) -> Result<RelatedMode> {
    let normalized = raw.unwrap_or("auto").trim().to_ascii_lowercase();
    match normalized.as_str() {
        "explore" => Ok(RelatedMode::Explore),
        "focus" => Ok(RelatedMode::Focus),
        "auto" => {
            if !docs_intent && matches!(query_type, QueryType::Identifier | QueryType::Path) {
                Ok(RelatedMode::Focus)
            } else {
                Ok(RelatedMode::Explore)
            }
        }
        other => Err(anyhow!(
            "related_mode must be 'explore' or 'focus' (got '{other}')"
        )),
    }
}

fn tokenize_focus_query(query: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        // English.
        "a",
        "an",
        "and",
        "are",
        "as",
        "at",
        "be",
        "by",
        "for",
        "from",
        "how",
        "in",
        "is",
        "it",
        "of",
        "on",
        "or",
        "that",
        "the",
        "this",
        "to",
        "what",
        "when",
        "where",
        "why",
        "with",
        // Common repo/path noise.
        "bin",
        "crates",
        "doc",
        "docs",
        "lib",
        "src",
        "test",
        "tests",
        // Common extensions.
        "c",
        "cpp",
        "go",
        "h",
        "hpp",
        "java",
        "js",
        "json",
        "md",
        "mdx",
        "py",
        "rs",
        "toml",
        "ts",
        "yaml",
        "yml",
        // Russian.
        "в",
        "для",
        "и",
        "или",
        "как",
        "на",
        "по",
        "почему",
        "что",
        "где",
        "зачем",
    ];

    let q = query.to_ascii_lowercase();
    let mut tokens = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for raw in q.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-') {
        let token = raw.trim();
        if token.is_empty() || token.len() < 2 {
            continue;
        }
        if STOPWORDS.contains(&token) {
            continue;
        }
        if seen.insert(token.to_string()) {
            tokens.push(token.to_string());
        }
        if tokens.len() >= 12 {
            break;
        }
    }
    tokens
}

fn related_query_hit(rc: &RelatedContext, query_tokens: &[String]) -> bool {
    if query_tokens.is_empty() {
        return false;
    }

    let file = rc.chunk.file_path.to_ascii_lowercase();
    if query_tokens.iter().any(|t| file.contains(t)) {
        return true;
    }

    if let Some(symbol) = rc.chunk.metadata.symbol_name.as_deref() {
        let symbol = symbol.to_ascii_lowercase();
        if query_tokens.iter().any(|t| symbol.contains(t)) {
            return true;
        }
    }

    let content = rc.chunk.content.to_ascii_lowercase();
    query_tokens.iter().any(|t| content.contains(t))
}

fn prepare_related_contexts(
    mut related: Vec<RelatedContext>,
    related_mode: RelatedMode,
    query_tokens: &[String],
) -> Vec<RelatedContext> {
    let explore_sort = |a: &RelatedContext, b: &RelatedContext| {
        b.relevance_score
            .total_cmp(&a.relevance_score)
            .then_with(|| a.distance.cmp(&b.distance))
            .then_with(|| a.chunk.file_path.cmp(&b.chunk.file_path))
            .then_with(|| a.chunk.start_line.cmp(&b.chunk.start_line))
    };

    match related_mode {
        RelatedMode::Explore => {
            related.sort_by(explore_sort);
            related
        }
        RelatedMode::Focus => {
            const MAX_DISTANCE_FOCUS: usize = 2;
            const FALLBACK_NON_HITS: usize = 2;

            if query_tokens.is_empty() {
                related.sort_by(explore_sort);
                return related;
            }

            related.retain(|rc| rc.distance <= MAX_DISTANCE_FOCUS);

            let mut hits: Vec<RelatedContext> = Vec::new();
            let mut misses: Vec<RelatedContext> = Vec::new();
            for rc in related {
                if related_query_hit(&rc, query_tokens) {
                    hits.push(rc);
                } else {
                    misses.push(rc);
                }
            }

            let fallback = if hits.is_empty() {
                misses.sort_by(explore_sort);
                misses.truncate(FALLBACK_NON_HITS);
                misses
            } else {
                misses.retain(|rc| rc.distance <= 1);
                misses.sort_by(explore_sort);
                misses.truncate(FALLBACK_NON_HITS);
                misses
            };

            let mut combined: Vec<(bool, RelatedContext)> =
                hits.into_iter().map(|rc| (true, rc)).collect();
            combined.extend(fallback.into_iter().map(|rc| (false, rc)));

            combined.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| b.1.relevance_score.total_cmp(&a.1.relevance_score))
                    .then_with(|| a.1.distance.cmp(&b.1.distance))
                    .then_with(|| a.1.chunk.file_path.cmp(&b.1.chunk.file_path))
                    .then_with(|| a.1.chunk.start_line.cmp(&b.1.chunk.start_line))
            });

            combined.into_iter().map(|(_, rc)| rc).collect()
        }
    }
}

fn prepare_context_pack_enriched(
    mut enriched: Vec<EnrichedResult>,
    limit: usize,
    prefer_code: bool,
    include_docs: bool,
) -> Vec<EnrichedResult> {
    if !include_docs {
        enriched.retain(|er| classify_path_kind(&er.primary.chunk.file_path) != DocumentKind::Docs);
    }

    fn kind_rank(kind: DocumentKind, prefer_code: bool) -> u8 {
        if prefer_code {
            match kind {
                DocumentKind::Code => 0,
                DocumentKind::Test => 1,
                DocumentKind::Config => 2,
                DocumentKind::Other => 3,
                DocumentKind::Docs => 4,
            }
        } else {
            match kind {
                DocumentKind::Docs => 0,
                DocumentKind::Code => 1,
                DocumentKind::Test => 2,
                DocumentKind::Config => 3,
                DocumentKind::Other => 4,
            }
        }
    }

    enriched.sort_by(|a, b| {
        let a_kind = classify_path_kind(&a.primary.chunk.file_path);
        let b_kind = classify_path_kind(&b.primary.chunk.file_path);
        kind_rank(a_kind, prefer_code)
            .cmp(&kind_rank(b_kind, prefer_code))
            .then_with(|| b.primary.score.total_cmp(&a.primary.score))
            .then_with(|| a.primary.chunk.file_path.cmp(&b.primary.chunk.file_path))
            .then_with(|| a.primary.chunk.start_line.cmp(&b.primary.chunk.start_line))
    });

    if !include_docs {
        for er in &mut enriched {
            er.related
                .retain(|rc| classify_path_kind(&rc.chunk.file_path) != DocumentKind::Docs);
        }
    }

    enriched.truncate(limit);
    enriched
}

fn pack_enriched_results(
    enriched: Vec<EnrichedResult>,
    profile: &SearchProfile,
    max_chars: usize,
    max_related_per_primary: usize,
    request_options: &crate::command::domain::RequestOptions,
    related_mode: RelatedMode,
    query_tokens: &[String],
) -> (Vec<ContextPackItem>, ContextPackBudget, usize) {
    let mut used_chars = 0usize;
    let mut truncated = false;
    let mut dropped_items = 0usize;
    let mut filtered_out = 0usize;

    let mut items: Vec<ContextPackItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let mut related_queues: Vec<VecDeque<RelatedContext>> = Vec::new();
    let mut selected_related: Vec<usize> = Vec::new();
    let mut per_relationship: Vec<HashMap<String, usize>> = Vec::new();

    for er in enriched {
        let primary = er.primary;
        let primary_id = primary.id.clone();
        if !seen.insert(primary_id.clone()) {
            continue;
        }
        if !crate::command::path_filters::path_allowed(&primary.chunk.file_path, request_options) {
            filtered_out += 1;
            continue;
        }

        let primary_item = ContextPackItem {
            id: primary_id,
            role: "primary".to_string(),
            file: primary.chunk.file_path.clone(),
            start_line: primary.chunk.start_line,
            end_line: primary.chunk.end_line,
            symbol: primary.chunk.metadata.symbol_name.clone(),
            chunk_type: primary
                .chunk
                .metadata
                .chunk_type
                .map(|ct| ct.as_str().to_string()),
            score: primary.score,
            imports: primary.chunk.metadata.context_imports.clone(),
            content: primary.chunk.content,
            relationship: None,
            distance: None,
        };
        let cost = estimate_item_chars(&primary_item);
        if used_chars.saturating_add(cost) > max_chars {
            truncated = true;
            dropped_items += 1;
            break;
        }
        used_chars += cost;
        items.push(primary_item);

        let mut related = er.related;
        related.retain(|rc| !profile.is_rejected(&rc.chunk.file_path));
        let before_filters = related.len();
        related.retain(|rc| {
            crate::command::path_filters::path_allowed(&rc.chunk.file_path, request_options)
        });
        filtered_out += before_filters.saturating_sub(related.len());
        let related = prepare_related_contexts(related, related_mode, query_tokens);
        related_queues.push(VecDeque::from(related));
        selected_related.push(0);
        per_relationship.push(HashMap::new());
    }

    fn relationship_cap(kind: &str) -> usize {
        match kind {
            "Calls" => 6,
            "Uses" => 6,
            "Contains" => 4,
            "Extends" => 3,
            "Imports" => 2,
            "TestedBy" => 2,
            _ => 2,
        }
    }

    'outer_related: while !truncated {
        let mut added_any = false;
        for idx in 0..related_queues.len() {
            if selected_related[idx] >= max_related_per_primary {
                continue;
            }

            let queue = &mut related_queues[idx];
            while let Some(rc) = queue.pop_front() {
                let kind = rc
                    .relationship_path
                    .first()
                    .cloned()
                    .unwrap_or_else(String::new);
                let cap = relationship_cap(&kind);
                let used = per_relationship[idx]
                    .get(kind.as_str())
                    .copied()
                    .unwrap_or(0);
                if used >= cap {
                    continue;
                }

                let id = format!(
                    "{}:{}:{}",
                    rc.chunk.file_path, rc.chunk.start_line, rc.chunk.end_line
                );
                if !seen.insert(id.clone()) {
                    continue;
                }

                let item = ContextPackItem {
                    id,
                    role: "related".to_string(),
                    file: rc.chunk.file_path.clone(),
                    start_line: rc.chunk.start_line,
                    end_line: rc.chunk.end_line,
                    symbol: rc.chunk.metadata.symbol_name.clone(),
                    chunk_type: rc
                        .chunk
                        .metadata
                        .chunk_type
                        .map(|ct| ct.as_str().to_string()),
                    score: rc.relevance_score,
                    imports: rc.chunk.metadata.context_imports.clone(),
                    content: rc.chunk.content,
                    relationship: Some(rc.relationship_path),
                    distance: Some(rc.distance),
                };

                let cost = estimate_item_chars(&item);
                if used_chars.saturating_add(cost) > max_chars {
                    truncated = true;
                    dropped_items += 1;
                    break 'outer_related;
                }

                used_chars += cost;
                items.push(item);
                *per_relationship[idx].entry(kind).or_insert(0) += 1;
                selected_related[idx] += 1;
                added_any = true;
                break;
            }
        }

        if !added_any {
            break;
        }
    }

    (
        items,
        ContextPackBudget {
            max_chars,
            used_chars,
            truncated,
            dropped_items,
            truncation: truncated.then_some(BudgetTruncation::MaxChars),
        },
        filtered_out,
    )
}

fn enforce_context_pack_budget(output: &mut ContextPackOutput) -> Result<()> {
    let max_chars = output.budget.max_chars;
    let used = enforce_max_chars(
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
            if !inner.items.is_empty() {
                inner.items.pop();
                inner.budget.dropped_items += 1;
                return true;
            }
            false
        },
    )
    .map_err(|_| {
        let min_chars = finalize_used_chars(output, |inner, used| inner.budget.used_chars = used)
            .unwrap_or(output.budget.used_chars);
        anyhow!("max_chars too small for context_pack response (min_chars={min_chars})")
    })?;
    output.budget.used_chars = used;
    Ok(())
}

fn estimate_item_chars(item: &ContextPackItem) -> usize {
    let imports: usize = item.imports.iter().map(|s| s.len() + 1).sum();
    item.content.len() + imports + 128
}

async fn load_or_build_graph_nodes_store(
    project_root: &Path,
    source_index_mtime_ms: u64,
    language: GraphLanguage,
    assembler: &ContextAssembler,
    max_neighbors_per_relation: usize,
    embedding: &context_vector_store::EmbeddingTemplates,
) -> Result<(GraphNodeStore, bool)> {
    let path = graph_nodes_path(project_root);
    let language_key = graph_language_key(language).to_string();
    let template_hash = embedding.graph_node_template_hash();

    if let Ok(store) = GraphNodeStore::load(&path).await {
        let meta = store.meta();
        if meta.source_index_mtime_ms == source_index_mtime_ms
            && meta.graph_language == language_key
            && meta.graph_doc_version == GRAPH_DOC_VERSION
            && meta.template_hash == template_hash
        {
            return Ok((store, true));
        }
    }

    let docs = build_graph_docs(
        assembler,
        GraphDocConfig {
            max_neighbors_per_relation,
        },
    );
    let docs: Vec<GraphNodeDoc> = docs
        .into_iter()
        .map(|doc| {
            let text = embedding.render_graph_node_doc(&doc.doc)?;
            Ok(GraphNodeDoc {
                node_id: doc.node_id,
                chunk_id: doc.chunk_id,
                text,
                doc_hash: doc.doc_hash,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let meta = GraphNodeStoreMeta::for_current_model(
        source_index_mtime_ms,
        language_key,
        GRAPH_DOC_VERSION,
        template_hash,
    )?;
    let store = GraphNodeStore::build_or_update(&path, meta, docs).await?;
    Ok((store, false))
}

fn graph_language_key(language: GraphLanguage) -> &'static str {
    match language {
        GraphLanguage::Rust => "rust",
        GraphLanguage::Python => "python",
        GraphLanguage::JavaScript => "javascript",
        GraphLanguage::TypeScript => "typescript",
    }
}

struct LoadedSemanticIndexes {
    sources: Vec<(String, VectorIndex)>,
    store_path: std::path::PathBuf,
    store_mtime: SystemTime,
    index_size_bytes: Option<u64>,
}

async fn load_semantic_indexes(
    root: &Path,
    profile: &SearchProfile,
) -> Result<LoadedSemanticIndexes> {
    let store_path = index_path(root);
    ensure_index_exists(&store_path)?;

    let store_mtime = load_store_mtime(&store_path).await?;
    let index_size_bytes = tokio::fs::metadata(&store_path).await.ok().map(|m| m.len());

    let default_model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());

    let mut requested: Vec<String> = Vec::new();
    requested.push(default_model_id.clone());
    requested.extend(semantic_model_roster(profile));

    let mut sources = Vec::new();
    let mut seen = HashSet::new();
    for model_id in requested {
        if !seen.insert(model_id.clone()) {
            continue;
        }
        let path = index_path_for_model(root, &model_id);
        if !path.exists() {
            continue;
        }
        let index = VectorIndex::load(&path)
            .await
            .with_context(|| format!("Failed to load index {}", path.display()))?;
        sources.push((model_id, index));
    }

    if sources.is_empty() {
        return Err(anyhow!(
            "No semantic indices available. Expected at least {}",
            store_path.display()
        ));
    }

    Ok(LoadedSemanticIndexes {
        sources,
        store_path,
        store_mtime,
        index_size_bytes,
    })
}

async fn load_chunk_corpus(root: &Path) -> Result<Option<ChunkCorpus>> {
    let path = corpus_path_for_project_root(root);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(ChunkCorpus::load(&path).await?))
}

fn semantic_model_roster(profile: &SearchProfile) -> Vec<String> {
    let experts = profile.experts();
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for kind in [
        QueryKind::Identifier,
        QueryKind::Path,
        QueryKind::Conceptual,
    ] {
        for model_id in experts.semantic_models(kind) {
            if seen.insert(model_id.clone()) {
                out.push(model_id.clone());
            }
        }
    }

    out
}

fn build_chunk_lookup(chunks: &[context_code_chunker::CodeChunk]) -> HashMap<String, usize> {
    let mut lookup = HashMap::new();
    for (idx, chunk) in chunks.iter().enumerate() {
        lookup.insert(
            format!(
                "{}:{}:{}",
                chunk.file_path, chunk.start_line, chunk.end_line
            ),
            idx,
        );
    }
    lookup
}

pub(crate) fn collect_chunks(
    store: &context_vector_store::VectorStore,
) -> (Vec<context_code_chunker::CodeChunk>, HashMap<String, usize>) {
    let mut chunks = Vec::new();
    let mut lookup = HashMap::new();

    for id in store.chunk_ids() {
        if let Some(stored) = store.get_chunk(&id) {
            lookup.insert(id.clone(), chunks.len());
            chunks.push(stored.chunk.clone());
        }
    }

    (chunks, lookup)
}

pub(crate) fn format_basic_output(result: SearchResult) -> SearchResultOutput {
    SearchResultOutput {
        file: result.chunk.file_path.clone(),
        start_line: result.chunk.start_line,
        end_line: result.chunk.end_line,
        symbol: result.chunk.metadata.symbol_name.clone(),
        chunk_type: result
            .chunk
            .metadata
            .chunk_type
            .map(|ct| ct.as_str().to_string()),
        score: result.score,
        content: result.chunk.content.clone(),
        context: result.chunk.metadata.context_imports.clone(),
        reason: Some(reason_label(&result)),
        related: None,
        graph: None,
        rationale: None,
    }
}

pub(crate) fn format_enriched_output(
    enriched: EnrichedResult,
    show_graph: bool,
    profile: &SearchProfile,
) -> SearchResultOutput {
    let EnrichedResult {
        primary,
        ref related,
        ..
    } = enriched;

    let related_outputs: Option<Vec<RelatedCodeOutput>> = if related.is_empty() {
        None
    } else {
        Some(
            related
                .iter()
                .filter(|rc| !profile.is_rejected(&rc.chunk.file_path))
                .sorted_by(|a, b| {
                    relation_priority(&a.relationship_path)
                        .cmp(&relation_priority(&b.relationship_path))
                })
                .map(|rc| crate::command::domain::RelatedCodeOutput {
                    file: rc.chunk.file_path.clone(),
                    start_line: rc.chunk.start_line,
                    end_line: rc.chunk.end_line,
                    symbol: rc.chunk.metadata.symbol_name.clone(),
                    relationship: rc.relationship_path.clone(),
                    distance: rc.distance,
                    relevance: rc.relevance_score,
                    graph_path: Some(rc.relationship_path.join(" -> ")),
                    reason: Some("graph".to_string()),
                })
                .collect(),
        )
    };

    let graph = if show_graph && !related.is_empty() {
        let primary_symbol = primary
            .chunk
            .metadata
            .symbol_name
            .as_deref()
            .unwrap_or("unknown")
            .to_string();
        Some(
            related
                .iter()
                .map(|rc| crate::command::domain::RelationshipOutput {
                    from: primary_symbol.clone(),
                    to: rc
                        .chunk
                        .metadata
                        .symbol_name
                        .as_deref()
                        .unwrap_or("unknown")
                        .to_string(),
                    relationship: rc.relationship_path.join(" → "),
                })
                .collect(),
        )
    } else {
        None
    };

    let rationale = if let Some(rel) = &related_outputs {
        if rel.len() == 1 {
            Some(format!("graph path: {}", rel[0].relationship.join(" → ")))
        } else {
            Some(format!("graph context: {} related nodes", rel.len()))
        }
    } else {
        None
    };

    SearchResultOutput {
        file: primary.chunk.file_path.clone(),
        start_line: primary.chunk.start_line,
        end_line: primary.chunk.end_line,
        symbol: primary.chunk.metadata.symbol_name.clone(),
        chunk_type: primary
            .chunk
            .metadata
            .chunk_type
            .map(|ct| ct.as_str().to_string()),
        score: primary.score,
        content: primary.chunk.content.clone(),
        context: primary.chunk.metadata.context_imports.clone(),
        reason: Some(
            if related.is_empty() {
                "semantic"
            } else {
                "semantic+graph"
            }
            .to_string(),
        ),
        related: related_outputs,
        graph,
        rationale,
    }
}

pub(crate) fn dedup_results(
    mut entries: Vec<SearchResultOutput>,
    profile: &SearchProfile,
) -> (Vec<SearchResultOutput>, usize) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut kept = Vec::with_capacity(entries.len());
    let mut dropped = 0;
    let mut merged = 0;

    // Only hard-filter rejected paths (target/, node_modules/, etc.). Noise is handled by scoring.
    entries.retain(|e| !profile.is_rejected(&e.file));

    // First pass: exact key dedup
    let mut tmp = Vec::with_capacity(entries.len());
    for entry in entries.drain(..) {
        let key = key_for(&entry);
        if seen.contains_key(&key) {
            dropped += 1;
            continue;
        }
        seen.insert(key, kept.len());
        tmp.push(entry);
    }

    // Second pass: merge overlapping spans within one file
    tmp.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.end_line.cmp(&b.end_line))
    });

    let mut current: Option<SearchResultOutput> = None;
    for entry in tmp {
        if let Some(mut cur) = current.take() {
            if cur.file == entry.file
                && (spans_overlap(
                    cur.start_line,
                    cur.end_line,
                    entry.start_line,
                    entry.end_line,
                ) || similar_chunks(&cur, &entry))
            {
                // merge spans; keep best score, concat context minimally
                merged += 1;
                cur.end_line = cur.end_line.max(entry.end_line);
                if entry.score > cur.score {
                    cur.score = entry.score;
                    cur.content = entry.content.clone();
                    cur.symbol = entry.symbol.clone();
                    cur.chunk_type = entry.chunk_type.clone();
                    cur.reason = entry.reason.clone();
                    cur.rationale = entry.rationale.clone().or(cur.rationale);
                }
                let mut ctx = cur.context;
                for c in entry.context {
                    if !ctx.contains(&c) {
                        ctx.push(c);
                    }
                }
                cur.context = ctx;
                current = Some(cur);
                // merging counts as dropped
                dropped += 1;
            } else {
                kept.push(cur);
                current = Some(entry);
            }
        } else {
            current = Some(entry);
        }
    }
    if let Some(cur) = current {
        kept.push(cur);
    }

    (kept, dropped + merged)
}

fn spans_overlap(a_start: usize, a_end: usize, b_start: usize, b_end: usize) -> bool {
    !(a_end < b_start || b_end < a_start)
}

fn similar_chunks(a: &SearchResultOutput, b: &SearchResultOutput) -> bool {
    if a.file != b.file {
        return false;
    }
    let line_gap = if a.start_line > b.end_line {
        a.start_line - b.end_line
    } else {
        b.start_line.saturating_sub(a.end_line)
    };
    if line_gap > 10 {
        return false;
    }

    let a_words = words_set(&a.content);
    let b_words = words_set(&b.content);
    if a_words.is_empty() || b_words.is_empty() {
        return false;
    }
    let inter = a_words.intersection(&b_words).count() as f32;
    let union = (a_words.len() + b_words.len()) as f32 - inter;
    let jaccard = if union > 0.0 { inter / union } else { 0.0 };
    jaccard >= 0.8
}

fn words_set(text: &str) -> std::collections::HashSet<String> {
    text.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

fn reason_label(_result: &SearchResult) -> String {
    // Without the query string here, default to semantic; context path will upgrade to semantic+graph.
    "semantic".to_string()
}

fn annotate_reasons(query: &str, results: &mut [SearchResultOutput]) {
    let q = query.to_lowercase();
    for r in results.iter_mut() {
        if r.content.to_lowercase().contains(&q)
            || r.symbol
                .as_ref()
                .map(|s| s.to_lowercase().contains(&q))
                .unwrap_or(false)
        {
            r.reason = Some("term+semantic".to_string());
        }
        if let Some(rel) = &r.related {
            if let Some(path) = rel.first() {
                r.rationale = Some(format!(
                    "graph path: {}",
                    truncate_path(&path.relationship.join(" -> "), 4)
                ));
            }
        }
    }
}

fn truncate_path(path: &str, max_segments: usize) -> String {
    let parts: Vec<&str> = path.split(" -> ").collect();
    if parts.len() <= max_segments {
        path.to_string()
    } else {
        let mut out = parts[..max_segments].join(" -> ");
        out.push_str(" …");
        out
    }
}

fn relation_priority(path: &[String]) -> std::cmp::Reverse<u8> {
    let score = path
        .first()
        .map(|s| s.to_ascii_lowercase())
        .map(|s| {
            if s.contains("calls") {
                3
            } else if s.contains("uses") || s.contains("tests") || s.contains("tested") {
                2
            } else {
                1
            }
        })
        .unwrap_or(0);
    std::cmp::Reverse(score)
}

fn choose_strategy(query: &str) -> (SearchStrategy, Option<String>) {
    let query_type = QueryClassifier::classify(query);
    let docs_intent = QueryClassifier::is_docs_intent(query);
    if !docs_intent && matches!(query_type, QueryType::Identifier | QueryType::Path) {
        return (
            SearchStrategy::Direct,
            Some("strategy auto: identifier/path -> direct (precise hits)".to_string()),
        );
    }

    let q = query.to_lowercase();
    let mut reason = None;
    let strategy =
        if q.contains("stack") || q.contains("error") || q.contains("panic") || q.contains("debug")
        {
            reason = Some("strategy auto: debug -> extended, reuse_graph".to_string());
            SearchStrategy::Extended
        } else if q.contains("refactor") || q.contains("rename") || q.contains("migrate") {
            reason = Some("strategy auto: refactor -> direct (precise hits)".to_string());
            SearchStrategy::Direct
        } else if q.contains("architecture")
            || q.contains("overview")
            || q.contains("map")
            || q.contains("entrypoint")
        {
            reason = Some("strategy auto: navigation -> extended (broader coverage)".to_string());
            SearchStrategy::Extended
        } else if q.contains("perf") || q.contains("latency") || q.contains("throughput") {
            reason = Some("strategy auto: perf -> deep (transitive graph)".to_string());
            SearchStrategy::Deep
        } else {
            SearchStrategy::Extended
        };
    (strategy, reason)
}

fn choose_task_hint(query: &str) -> (Option<String>, Option<String>) {
    let q = query.to_lowercase();
    if q.contains("stack") || q.contains("error") || q.contains("panic") || q.contains("debug") {
        (
            Some("task: debug — focus on stack traces, extended strategy".to_string()),
            None,
        )
    } else if q.contains("refactor") || q.contains("rename") || q.contains("migrate") {
        (
            Some("task: refactor — prefer precise matches, consider direct strategy".to_string()),
            None,
        )
    } else if q.contains("architecture")
        || q.contains("overview")
        || q.contains("map")
        || q.contains("entrypoint")
    {
        (
            Some("task: navigation — broaden search, include graph context".to_string()),
            None,
        )
    } else if q.contains("perf") || q.contains("latency") || q.contains("throughput") {
        (
            Some("task: performance — deep graph context may help locate hot paths".to_string()),
            None,
        )
    } else {
        (None, None)
    }
}

pub(crate) fn trace_results(query: &str, results: &[SearchResultOutput]) {
    eprintln!("[trace] query=\"{}\" hits={}", query, results.len());
    for (idx, result) in results.iter().enumerate() {
        eprintln!(
            "[trace] #{:02} score={:.3} file={} lines {}-{}",
            idx + 1,
            result.score,
            result.file,
            result.start_line,
            result.end_line
        );
    }
}

pub(crate) fn parse_graph_language(value: &str) -> Result<GraphLanguage> {
    match value.to_lowercase().as_str() {
        "rust" => Ok(GraphLanguage::Rust),
        "python" => Ok(GraphLanguage::Python),
        "javascript" => Ok(GraphLanguage::JavaScript),
        "typescript" => Ok(GraphLanguage::TypeScript),
        other => Err(anyhow!("Unsupported graph language: {other}")),
    }
}

pub(crate) fn overlap_ratio(
    limit: usize,
    baseline: &HashSet<String>,
    context: &HashSet<String>,
) -> f32 {
    let overlap = baseline.intersection(context).count();
    if limit > 0 {
        overlap as f32 / limit as f32
    } else {
        0.0
    }
}

pub(crate) fn key_for(result: &SearchResultOutput) -> String {
    format!("{}:{}:{}", result.file, result.start_line, result.end_line)
}

#[cfg(test)]
mod tests {
    use super::{pack_enriched_results, prepare_context_pack_enriched, RelatedMode};
    use context_code_chunker::{ChunkMetadata, CodeChunk};
    use context_graph::AssemblyStrategy;
    use context_search::{EnrichedResult, RelatedContext, SearchProfile};
    use context_vector_store::SearchResult;

    fn chunk(path: &str, line: usize, content: &str) -> CodeChunk {
        CodeChunk::new(
            path.to_string(),
            line,
            line,
            content.to_string(),
            ChunkMetadata::default(),
        )
    }

    #[test]
    fn packer_applies_per_relationship_caps() {
        let profile = SearchProfile::general();

        let primary_chunk = chunk("src/main.rs", 1, "fn main() {}");
        let primary = SearchResult {
            id: "src/main.rs:1:1".to_string(),
            chunk: primary_chunk,
            score: 1.0,
        };

        let related: Vec<RelatedContext> = (0..5)
            .map(|idx| RelatedContext {
                chunk: chunk(&format!("src/imp{idx}.rs"), 1, "use x;"),
                relationship_path: vec!["Imports".to_string()],
                distance: 1,
                relevance_score: 10.0 - idx as f32,
            })
            .collect();

        let enriched = vec![EnrichedResult {
            primary,
            related,
            total_lines: 1,
            strategy: AssemblyStrategy::Extended,
        }];

        let request_options = crate::command::domain::RequestOptions::default();
        let query_tokens = Vec::new();
        let (items, budget, _filtered_out) = pack_enriched_results(
            enriched,
            &profile,
            50_000,
            100,
            &request_options,
            RelatedMode::Explore,
            &query_tokens,
        );
        assert!(!budget.truncated);

        let related_ids: Vec<String> = items
            .iter()
            .filter(|i| i.role == "related")
            .map(|i| i.id.clone())
            .collect();

        // "Imports" relationship is capped at 2 per primary.
        assert_eq!(related_ids.len(), 2);
        assert_eq!(related_ids[0], "src/imp0.rs:1:1");
        assert_eq!(related_ids[1], "src/imp1.rs:1:1");
    }

    #[test]
    fn packer_applies_path_filters_to_primary_items() {
        let profile = SearchProfile::general();

        let primary_a = SearchResult {
            id: "src/main.rs:1:1".to_string(),
            chunk: chunk("src/main.rs", 1, "fn main() {}"),
            score: 1.0,
        };
        let primary_b = SearchResult {
            id: "docs/readme.md:1:1".to_string(),
            chunk: chunk("docs/readme.md", 1, "# docs"),
            score: 0.9,
        };

        let enriched = vec![
            EnrichedResult {
                primary: primary_a,
                related: Vec::new(),
                total_lines: 1,
                strategy: AssemblyStrategy::Extended,
            },
            EnrichedResult {
                primary: primary_b,
                related: Vec::new(),
                total_lines: 1,
                strategy: AssemblyStrategy::Extended,
            },
        ];

        let request_options = crate::command::domain::RequestOptions {
            include_paths: vec!["src".to_string()],
            ..Default::default()
        };
        let query_tokens = Vec::new();
        let (items, budget, filtered_out) = pack_enriched_results(
            enriched,
            &profile,
            50_000,
            100,
            &request_options,
            RelatedMode::Explore,
            &query_tokens,
        );
        assert!(!budget.truncated);
        assert!(filtered_out >= 1);

        let primary_files: Vec<&str> = items
            .iter()
            .filter(|i| i.role == "primary")
            .map(|i| i.file.as_str())
            .collect();
        assert_eq!(primary_files, vec!["src/main.rs"]);
    }

    #[test]
    fn packer_focus_prefers_query_hits_over_raw_relevance() {
        let profile = SearchProfile::general();

        let primary = SearchResult {
            id: "src/main.rs:1:1".to_string(),
            chunk: chunk("src/main.rs", 1, "fn main() {}"),
            score: 1.0,
        };

        let related_miss = RelatedContext {
            chunk: chunk("src/miss.rs", 1, "fn unrelated() {}"),
            relationship_path: vec!["Calls".to_string()],
            distance: 1,
            relevance_score: 100.0,
        };
        let related_hit = RelatedContext {
            chunk: chunk("src/hit.rs", 1, "fn target() {}"),
            relationship_path: vec!["Calls".to_string()],
            distance: 1,
            relevance_score: 0.1,
        };

        let enriched = vec![EnrichedResult {
            primary,
            related: vec![related_miss, related_hit],
            total_lines: 1,
            strategy: AssemblyStrategy::Extended,
        }];

        let request_options = crate::command::domain::RequestOptions::default();
        let query_tokens = vec!["target".to_string()];
        let (items, budget, _filtered_out) = pack_enriched_results(
            enriched,
            &profile,
            50_000,
            100,
            &request_options,
            RelatedMode::Focus,
            &query_tokens,
        );
        assert!(!budget.truncated);

        let related_files: Vec<&str> = items
            .iter()
            .filter(|i| i.role == "related")
            .map(|i| i.file.as_str())
            .collect();
        assert_eq!(related_files, vec!["src/hit.rs", "src/miss.rs"]);
    }

    #[test]
    fn prepare_excludes_docs_when_disabled() {
        let primary_a = SearchResult {
            id: "src/main.rs:1:1".to_string(),
            chunk: chunk("src/main.rs", 1, "fn main() {}"),
            score: 0.9,
        };
        let primary_b = SearchResult {
            id: "docs/readme.md:1:1".to_string(),
            chunk: chunk("docs/readme.md", 1, "# docs"),
            score: 1.0,
        };

        let enriched = vec![
            EnrichedResult {
                primary: primary_b,
                related: Vec::new(),
                total_lines: 1,
                strategy: AssemblyStrategy::Extended,
            },
            EnrichedResult {
                primary: primary_a,
                related: Vec::new(),
                total_lines: 1,
                strategy: AssemblyStrategy::Extended,
            },
        ];

        let prepared = prepare_context_pack_enriched(enriched, 10, false, false);
        let files: Vec<&str> = prepared
            .iter()
            .map(|er| er.primary.chunk.file_path.as_str())
            .collect();
        assert_eq!(files, vec!["src/main.rs"]);
    }

    #[test]
    fn prepare_prefers_code_over_docs_when_enabled() {
        let primary_a = SearchResult {
            id: "src/main.rs:1:1".to_string(),
            chunk: chunk("src/main.rs", 1, "fn main() {}"),
            score: 0.9,
        };
        let primary_b = SearchResult {
            id: "docs/readme.md:1:1".to_string(),
            chunk: chunk("docs/readme.md", 1, "# docs"),
            score: 1.0,
        };

        let enriched = vec![
            EnrichedResult {
                primary: primary_b,
                related: Vec::new(),
                total_lines: 1,
                strategy: AssemblyStrategy::Extended,
            },
            EnrichedResult {
                primary: primary_a,
                related: Vec::new(),
                total_lines: 1,
                strategy: AssemblyStrategy::Extended,
            },
        ];

        let prepared = prepare_context_pack_enriched(enriched, 10, true, true);
        let files: Vec<&str> = prepared
            .iter()
            .map(|er| er.primary.chunk.file_path.as_str())
            .collect();
        assert_eq!(files, vec!["src/main.rs", "docs/readme.md"]);
    }
}
