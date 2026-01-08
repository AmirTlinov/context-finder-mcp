use crate::error::{Result, SearchError};
use crate::fusion::{AstBooster, RRFFusion};
use crate::fuzzy::FuzzySearch;
use crate::profile::SearchProfile;
use crate::query_classifier::{QueryClassifier, QueryType};
use crate::query_expansion::QueryExpander;
use crate::rerank::rerank_candidates;
use context_code_chunker::CodeChunk;
use context_graph::{AssemblyStrategy, ContextAssembler, GraphBuilder, GraphLanguage};
use context_vector_store::ChunkCorpus;
use context_vector_store::ModelRegistry;
use context_vector_store::{QueryKind, SearchResult, VectorIndex};
use std::collections::{HashMap, HashSet};

struct SemanticSource {
    index: VectorIndex,
}

#[derive(Clone, Debug)]
enum SemanticSearchStatus {
    Enabled,
    Disabled { reason: String },
}

impl SemanticSearchStatus {
    fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled)
    }

    fn disabled_reason(&self) -> Option<&str> {
        match self {
            Self::Disabled { reason } => Some(reason.as_str()),
            Self::Enabled => None,
        }
    }
}

/// Hybrid search combining semantic (multi-model) + fuzzy + RRF fusion.
///
/// This searcher keeps the same output shape as `HybridSearch`, but uses multiple semantic experts
/// (embedding models + corresponding indices) selected by `SearchProfile::experts()`.
pub struct MultiModelHybridSearch {
    sources: HashMap<String, SemanticSource>,
    chunks: Vec<CodeChunk>,
    chunk_id_to_idx: HashMap<String, usize>,
    rejected: Vec<bool>,
    fuzzy: FuzzySearch,
    fusion: RRFFusion,
    expander: QueryExpander,
    profile: SearchProfile,
    registry: ModelRegistry,
    semantic: SemanticSearchStatus,
}

impl MultiModelHybridSearch {
    pub fn from_env(sources: Vec<(String, VectorIndex)>, profile: SearchProfile) -> Result<Self> {
        Self::new(sources, profile, ModelRegistry::from_env()?)
    }

    pub fn from_env_with_corpus(
        sources: Vec<(String, VectorIndex)>,
        profile: SearchProfile,
        corpus: ChunkCorpus,
    ) -> Result<Self> {
        Self::new_with_corpus(sources, profile, ModelRegistry::from_env()?, corpus)
    }

    pub fn new(
        sources: Vec<(String, VectorIndex)>,
        profile: SearchProfile,
        registry: ModelRegistry,
    ) -> Result<Self> {
        if sources.is_empty() {
            return Err(SearchError::Other(
                "Multi-model search requires at least one semantic index".to_string(),
            ));
        }

        let mut by_id = HashMap::new();
        for (model_id, index) in sources {
            let key = model_id.trim().to_string();
            if key.is_empty() {
                continue;
            }
            by_id.insert(key.clone(), SemanticSource { index });
        }

        if by_id.is_empty() {
            return Err(SearchError::Other(
                "Multi-model search requires at least one semantic index".to_string(),
            ));
        }

        // Canonical chunk order is derived from the lexicographically-sorted chunk ids of the
        // first available index. This keeps results deterministic and compatible with rerank logic.
        let canonical_index = {
            let mut ids: Vec<&String> = by_id.keys().collect();
            ids.sort();
            let canonical_id = ids.first().copied().expect("by_id is non-empty").as_str();
            &by_id
                .get(canonical_id)
                .expect("canonical id must exist")
                .index
        };
        let (chunks, chunk_id_to_idx) = collect_chunks(canonical_index);
        if chunks.is_empty() {
            return Err(SearchError::Other(
                "Semantic indices contain no stored chunks; load with chunk corpus (IndexBundle v3)"
                    .to_string(),
            ));
        }
        let rejected: Vec<bool> = chunks
            .iter()
            .map(|c| profile.is_rejected(&c.file_path))
            .collect();

        Ok(Self {
            sources: by_id,
            chunks,
            chunk_id_to_idx,
            rejected,
            fuzzy: FuzzySearch::new(),
            fusion: RRFFusion::default(),
            expander: QueryExpander::new(),
            profile,
            registry,
            semantic: SemanticSearchStatus::Enabled,
        })
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn new_with_corpus(
        sources: Vec<(String, VectorIndex)>,
        profile: SearchProfile,
        registry: ModelRegistry,
        corpus: ChunkCorpus,
    ) -> Result<Self> {
        if sources.is_empty() {
            return Err(SearchError::Other(
                "Multi-model search requires at least one semantic index".to_string(),
            ));
        }

        let mut by_id = HashMap::new();
        for (model_id, index) in sources {
            let key = model_id.trim().to_string();
            if key.is_empty() {
                continue;
            }
            by_id.insert(key.clone(), SemanticSource { index });
        }

        if by_id.is_empty() {
            return Err(SearchError::Other(
                "Multi-model search requires at least one semantic index".to_string(),
            ));
        }

        let canonical_index = {
            let mut ids: Vec<&String> = by_id.keys().collect();
            ids.sort();
            let canonical_id = ids.first().copied().expect("by_id is non-empty").as_str();
            &by_id
                .get(canonical_id)
                .expect("canonical id must exist")
                .index
        };

        let (chunks, chunk_id_to_idx) = collect_chunks_from_corpus(&corpus, canonical_index);
        if chunks.is_empty() {
            return Err(SearchError::Other(
                "Chunk corpus is empty or does not match indexed chunk ids".to_string(),
            ));
        }

        let rejected: Vec<bool> = chunks
            .iter()
            .map(|c| profile.is_rejected(&c.file_path))
            .collect();

        Ok(Self {
            sources: by_id,
            chunks,
            chunk_id_to_idx,
            rejected,
            fuzzy: FuzzySearch::new(),
            fusion: RRFFusion::default(),
            expander: QueryExpander::new(),
            profile,
            registry,
            semantic: SemanticSearchStatus::Enabled,
        })
    }

    #[must_use]
    pub fn chunks(&self) -> &[CodeChunk] {
        &self.chunks
    }

    #[must_use]
    pub fn loaded_model_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.sources.keys().cloned().collect();
        ids.sort();
        ids
    }

    #[must_use]
    pub fn has_semantic_index(&self, model_id: &str) -> bool {
        let key = model_id.trim();
        !key.is_empty() && self.sources.contains_key(key)
    }

    pub fn insert_semantic_index(&mut self, model_id: String, index: VectorIndex) {
        let key = model_id.trim().to_string();
        if key.is_empty() {
            return;
        }

        // Guardrail: mismatched embedding dimensions can happen if users swap models without
        // rebuilding indices; fail safely by disabling semantic until corrected.
        if let Some(existing) = self.sources.values().next() {
            if existing.index.dimension() != index.dimension() {
                self.semantic = SemanticSearchStatus::Disabled {
                    reason: format!(
                        "Semantic index dimension mismatch for model '{key}' (got {}, expected {})",
                        index.dimension(),
                        existing.index.dimension()
                    ),
                };
                return;
            }
        }

        self.sources.insert(key, SemanticSource { index });
    }

    pub fn remove_semantic_index(&mut self, model_id: &str) {
        let key = model_id.trim();
        if key.is_empty() {
            return;
        }
        // Never remove the last remaining index: many call paths assume at least one semantic
        // source exists (even if semantic is later disabled due to embedding issues).
        if self.sources.len() <= 1 {
            return;
        }
        self.sources.remove(key);
    }

    #[must_use]
    pub fn semantic_disabled_reason(&self) -> Option<&str> {
        self.semantic.disabled_reason()
    }

    pub async fn search(&mut self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let query = query.trim();
        if query.is_empty() {
            return Err(SearchError::EmptyQuery);
        }

        if let Some(results) = self.try_direct_file_path(query, limit) {
            return Ok(results);
        }

        if let Some(anchor) = Self::extract_symbol_anchor(query) {
            if anchor != query {
                if let Some(results) = self.try_direct_symbol_match(&anchor, limit) {
                    return Ok(results);
                }
            }
        }

        if let Some(results) = self.try_direct_symbol_match(query, limit) {
            return Ok(results);
        }

        // Expand query with synonyms and variants
        let expanded_query = self.expander.expand_to_query(query);
        let anchor = Self::extract_symbol_anchor(query).map(|a| self.expander.expand_to_query(&a));

        let weights = QueryClassifier::weights(query);
        let candidate_pool = candidate_pool(limit, weights.candidate_multiplier);
        let tokens = crate::hybrid::query_tokens(query);
        let query_type = QueryClassifier::classify(query);
        let query_kind = match query_type {
            QueryType::Identifier => QueryKind::Identifier,
            QueryType::Path => QueryKind::Path,
            QueryType::Conceptual => QueryKind::Conceptual,
        };

        // 1) Multi-model semantic search (rank-fused), keeping per-chunk max cosine for rerank.
        // If embeddings are unavailable (e.g. CUDA EP missing and CPU fallback not allowed),
        // degrade gracefully to fuzzy-only search. This keeps the tool useful instead of failing.
        let (semantic_rank, semantic_map) = if self.semantic.is_enabled() {
            let embedding_base = if query_kind == QueryKind::Identifier {
                anchor.as_deref().unwrap_or(expanded_query.as_str())
            } else {
                expanded_query.as_str()
            };
            let embedding_query = self
                .profile
                .embedding()
                .render_query(query_kind, embedding_base)?;

            match self
                .semantic_search_multi(query, query_kind, &embedding_query, candidate_pool)
                .await
            {
                Ok(result) => result,
                Err(err) => {
                    if let Some(reason) = semantic_disable_reason(&err) {
                        log::warn!(
                            "Semantic search disabled; falling back to fuzzy-only results: {reason}"
                        );
                        self.semantic = SemanticSearchStatus::Disabled { reason };
                        (Vec::new(), HashMap::new())
                    } else {
                        return Err(err);
                    }
                }
            }
        } else {
            (Vec::new(), HashMap::new())
        };

        // 2) Fuzzy search (path/symbol matching)
        let min_fuzzy = self.profile.min_fuzzy_score();
        let fuzzy_query = if query_kind == QueryKind::Identifier {
            anchor.as_deref().unwrap_or(query)
        } else {
            query
        };
        let fuzzy_scores = filter_fuzzy(
            self.fuzzy.search(fuzzy_query, &self.chunks, candidate_pool),
            &self.rejected,
            min_fuzzy,
        );
        let fuzzy_map: HashMap<usize, f32> = fuzzy_scores.iter().copied().collect();

        // 3) RRF Fusion with adaptive weights based on query type
        let fused_scores =
            self.fusion
                .fuse_adaptive(query, &weights, &semantic_rank, &fuzzy_scores);

        // 4) AST-aware boosting + rule-based rerank
        let boosted_scores = rerank_candidates(
            &self.profile,
            &self.chunks,
            &tokens,
            AstBooster::boost(&self.chunks, fused_scores),
            &semantic_map,
            &fuzzy_map,
        );

        // 5) Convert to SearchResult using chunk indices
        let mut final_results: Vec<SearchResult> = boosted_scores
            .into_iter()
            .filter_map(|(idx, score)| {
                self.chunks.get(idx).map(|chunk| {
                    let id = format!(
                        "{}:{}:{}",
                        chunk.file_path, chunk.start_line, chunk.end_line
                    );
                    let weight = match query_type {
                        QueryType::Conceptual => self.profile.path_weight(&chunk.file_path),
                        QueryType::Identifier | QueryType::Path => {
                            self.profile.path_boost_weight(&chunk.file_path)
                        }
                    };
                    let penalized = score * weight;
                    SearchResult {
                        chunk: chunk.clone(),
                        score: penalized,
                        id,
                    }
                })
            })
            .collect();

        // 6) Normalize scores to 0-1 range
        crate::hybrid::HybridSearch::normalize_scores(&mut final_results);

        // Sort by final score descending with deterministic tiebreaker.
        final_results.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        final_results.truncate(limit);

        Ok(final_results)
    }

    fn try_direct_file_path(&self, query: &str, limit: usize) -> Option<Vec<SearchResult>> {
        if !matches!(QueryClassifier::classify(query), QueryType::Path) {
            return None;
        }

        // Only do a strict direct match when the query is a single token that looks like a file
        // path (contains a separator or an extension). For module-like queries (e.g. `foo::bar`)
        // or multi-token queries, keep the standard hybrid pipeline.
        if query.split_whitespace().count() != 1 {
            return None;
        }
        if !(query.contains('/') || query.contains('\\') || has_file_extension(query)) {
            return None;
        }

        let needle = normalize_path_query(query);
        if needle.is_empty() {
            return None;
        }

        let mut exact: Vec<usize> = Vec::new();
        let mut suffix: Vec<usize> = Vec::new();
        let mut contains: Vec<usize> = Vec::new();

        for (idx, chunk) in self.chunks.iter().enumerate() {
            if self.rejected.get(idx).copied().unwrap_or(false) {
                continue;
            }
            let path = normalize_path_query(&chunk.file_path);
            if path == needle {
                exact.push(idx);
            } else if path.ends_with(&needle) {
                suffix.push(idx);
            } else if path.contains(&needle) {
                contains.push(idx);
            }
        }

        let mut hits = if !exact.is_empty() {
            exact
        } else if !suffix.is_empty() {
            suffix
        } else if !contains.is_empty() {
            contains
        } else {
            return None;
        };

        hits.sort_by(|&a, &b| {
            self.chunks[a]
                .file_path
                .cmp(&self.chunks[b].file_path)
                .then_with(|| self.chunks[a].start_line.cmp(&self.chunks[b].start_line))
                .then_with(|| self.chunks[a].end_line.cmp(&self.chunks[b].end_line))
        });
        hits.truncate(limit.max(1));

        let results = hits
            .into_iter()
            .enumerate()
            .filter_map(|(rank, idx)| {
                let chunk = self.chunks.get(idx)?.clone();
                let id = format!(
                    "{}:{}:{}",
                    chunk.file_path, chunk.start_line, chunk.end_line
                );
                #[allow(clippy::cast_precision_loss)]
                let score = (rank as f32).mul_add(-1e-3, 1.0).max(0.0);
                Some(SearchResult { chunk, score, id })
            })
            .collect();

        Some(results)
    }

    fn try_direct_symbol_match(&self, query: &str, limit: usize) -> Option<Vec<SearchResult>> {
        if !matches!(QueryClassifier::classify(query), QueryType::Identifier) {
            return None;
        }
        if query.split_whitespace().count() != 1 {
            return None;
        }

        let needle = query.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return None;
        }

        let mut hits: Vec<usize> = Vec::new();
        for (idx, chunk) in self.chunks.iter().enumerate() {
            if self.rejected.get(idx).copied().unwrap_or(false) {
                continue;
            }
            let Some(symbol) = chunk.metadata.symbol_name.as_ref() else {
                continue;
            };
            if symbol.to_ascii_lowercase() == needle {
                hits.push(idx);
            }
        }

        if hits.is_empty() {
            return None;
        }

        hits.sort_by(|&a, &b| {
            self.chunks[a]
                .file_path
                .cmp(&self.chunks[b].file_path)
                .then_with(|| self.chunks[a].start_line.cmp(&self.chunks[b].start_line))
                .then_with(|| self.chunks[a].end_line.cmp(&self.chunks[b].end_line))
        });
        hits.truncate(limit.max(1));

        let results = hits
            .into_iter()
            .enumerate()
            .filter_map(|(rank, idx)| {
                let chunk = self.chunks.get(idx)?.clone();
                let id = format!(
                    "{}:{}:{}",
                    chunk.file_path, chunk.start_line, chunk.end_line
                );
                #[allow(clippy::cast_precision_loss)]
                let score = (rank as f32).mul_add(-1e-3, 1.0).max(0.0);
                Some(SearchResult { chunk, score, id })
            })
            .collect();

        Some(results)
    }

    async fn semantic_search_multi(
        &self,
        raw_query: &str,
        query_kind: QueryKind,
        embedding_query: &str,
        limit: usize,
    ) -> Result<(Vec<(usize, f32)>, HashMap<usize, f32>)> {
        let desired_models = self.profile.experts().semantic_models(query_kind);
        let mut models: Vec<&str> = desired_models
            .iter()
            .map(String::as_str)
            .filter(|id| self.sources.contains_key(*id))
            .collect();

        if models.is_empty() {
            // Fallback: use any available model index to avoid hard-failing.
            let mut available: Vec<&str> = self.sources.keys().map(String::as_str).collect();
            available.sort_unstable();
            if let Some(first) = available.first().copied() {
                models.push(first);
            }
        }

        if models.is_empty() {
            return Err(SearchError::Other(
                "No semantic indices available for multi-model search".to_string(),
            ));
        }

        if query_kind == QueryKind::Conceptual && models.len() <= 2 {
            models = pick_single_conceptual_model(&models, raw_query);
        }

        // Embed queries per model first so we can run index search without holding any locks.
        let mut embeds: Vec<(&str, Vec<f32>)> = Vec::with_capacity(models.len());
        let mut first_embed_error: Option<context_vector_store::VectorStoreError> = None;
        for &model_id in &models {
            match self.registry.embed(model_id, embedding_query).await {
                Ok(query_vec) => {
                    embeds.push((model_id, query_vec));
                }
                Err(err) => {
                    log::debug!("Semantic embed failed for model '{model_id}': {err}");
                    if first_embed_error.is_none() {
                        first_embed_error = Some(err);
                    }
                }
            }
        }

        // If no models could embed the query, treat semantic search as unavailable and let the
        // caller decide how to degrade (typically: fuzzy-only search).
        if embeds.is_empty() {
            return Err(SearchError::VectorStoreError(
                first_embed_error.unwrap_or_else(|| {
                    context_vector_store::VectorStoreError::EmbeddingError(
                        "No embedding models available to embed query".to_string(),
                    )
                }),
            ));
        }

        // Rank lists per model (idx order) + max cosine map for rerank thresholds.
        let mut per_model_ranks: Vec<Vec<usize>> = Vec::with_capacity(models.len());
        let mut semantic_max: HashMap<usize, f32> = HashMap::new();

        for (model_id, query_vec) in embeds {
            let Some(source) = self.sources.get(model_id) else {
                continue;
            };

            // Search by vector; map ids back to canonical chunk indices.
            let hits = source.index.search_ids_by_vector(&query_vec, limit)?;
            let mut rank = Vec::new();
            let mut seen: HashSet<usize> = HashSet::new();
            for (chunk_id, score) in hits {
                let Some(&idx) = self.chunk_id_to_idx.get(&chunk_id) else {
                    continue;
                };
                if self.rejected.get(idx).copied().unwrap_or(false) {
                    continue;
                }
                if !seen.insert(idx) {
                    continue;
                }
                rank.push(idx);
                semantic_max
                    .entry(idx)
                    .and_modify(|v| *v = v.max(score))
                    .or_insert(score);
            }

            per_model_ranks.push(rank);
        }

        if per_model_ranks.is_empty() {
            return Ok((Vec::new(), semantic_max));
        }

        // Fuse per-model rankings using RRF (rank-only), then use the fused order for the semantic
        // list passed into the main fusion stage. Scores are ignored by RRF; we keep cosine values
        // for downstream thresholding via `semantic_max`.
        let fused = fuse_rrf(&per_model_ranks, 60.0);
        let semantic_rank: Vec<(usize, f32)> = fused
            .into_iter()
            .filter_map(|idx| semantic_max.get(&idx).copied().map(|score| (idx, score)))
            .take(limit)
            .collect();

        Ok((semantic_rank, semantic_max))
    }

    fn extract_symbol_anchor(query: &str) -> Option<String> {
        let mut best: Option<(usize, String)> = None;
        for raw in query.split_whitespace() {
            let token = Self::strip_symbol_punct(raw);
            if token.is_empty() {
                continue;
            }
            let token = token.strip_suffix("()").unwrap_or(token);
            let token = token.rsplit_once("::").map_or(token, |(_, tail)| tail);
            if token.is_empty() {
                continue;
            }
            if !Self::is_identifierish(token) {
                continue;
            }

            let score = Self::anchor_score(token);
            match best.as_ref() {
                Some((best_score, _)) if *best_score >= score => {}
                _ => best = Some((score, token.to_string())),
            }
        }
        best.map(|(_, token)| token)
    }

    fn strip_symbol_punct(token: &str) -> &str {
        token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != ':')
    }

    fn is_identifierish(token: &str) -> bool {
        let has_snake = token.contains('_');
        let has_digits = token.chars().any(|c| c.is_ascii_digit());
        let has_mixed_case = token.chars().any(|c| c.is_ascii_lowercase())
            && token.chars().any(|c| c.is_ascii_uppercase());
        has_snake || has_digits || has_mixed_case
    }

    fn anchor_score(token: &str) -> usize {
        let mut score = token.len();
        if token.contains('_') {
            score += 50;
        }
        if token.chars().any(|c| c.is_ascii_digit()) {
            score += 20;
        }
        if token.chars().any(|c| c.is_ascii_lowercase())
            && token.chars().any(|c| c.is_ascii_uppercase())
        {
            score += 30;
        }
        score
    }
}

fn pick_single_conceptual_model<'a>(candidates: &[&'a str], query: &str) -> Vec<&'a str> {
    let has_cyrillic = query
        .chars()
        .any(|c| matches!(c as u32, 0x0400..=0x052F | 0x2DE0..=0x2DFF | 0xA640..=0xA69F));

    if has_cyrillic {
        if let Some(m) = candidates
            .iter()
            .copied()
            .find(|id| *id == "multilingual-e5-small")
        {
            return vec![m];
        }
    }

    if let Some(m) = candidates
        .iter()
        .copied()
        .find(|id| *id == "embeddinggemma-300m")
    {
        return vec![m];
    }

    candidates
        .first()
        .copied()
        .map_or_else(Vec::new, |m| vec![m])
}

/// Context-aware search with automatic related code assembly, backed by `MultiModelHybridSearch`.
pub struct MultiModelContextSearch {
    hybrid: MultiModelHybridSearch,
    assembler: Option<ContextAssembler>,
}

impl MultiModelContextSearch {
    pub const fn new(hybrid: MultiModelHybridSearch) -> Result<Self> {
        Ok(Self {
            hybrid,
            assembler: None,
        })
    }

    pub fn set_assembler(&mut self, assembler: ContextAssembler) {
        self.assembler = Some(assembler);
    }

    #[must_use]
    pub const fn assembler(&self) -> Option<&ContextAssembler> {
        self.assembler.as_ref()
    }

    pub fn build_graph(&mut self, language: GraphLanguage) -> Result<()> {
        let chunks: Vec<CodeChunk> = self.hybrid.chunks().to_vec();
        let mut builder = GraphBuilder::new(language)?;
        let graph = builder.build(&chunks)?;
        self.assembler = Some(ContextAssembler::new(graph));
        Ok(())
    }

    /// Get graph statistics (nodes, edges) if a graph has been built or loaded.
    #[must_use]
    pub fn graph_stats(&self) -> Option<(usize, usize)> {
        self.assembler.as_ref().map(|a| {
            let stats = a.get_stats();
            (stats.total_nodes, stats.total_edges)
        })
    }

    #[allow(clippy::similar_names)]
    pub async fn search_with_context(
        &mut self,
        query: &str,
        limit: usize,
        strategy: AssemblyStrategy,
    ) -> Result<Vec<crate::context_search::EnrichedResult>> {
        let results = self.hybrid.search(query, limit).await?;

        let Some(assembler) = &self.assembler else {
            return Ok(results
                .into_iter()
                .map(|r| crate::context_search::EnrichedResult {
                    total_lines: r.chunk.line_count(),
                    primary: r,
                    related: vec![],
                    strategy,
                })
                .collect());
        };

        let mut enriched = Vec::new();
        for result in results {
            let chunk_id = &result.id;
            match assembler.assemble_for_chunk(chunk_id, strategy) {
                Ok(assembled) => {
                    let related = assembled
                        .related_chunks
                        .into_iter()
                        .map(|rc| crate::context_search::RelatedContext {
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
                    enriched.push(crate::context_search::EnrichedResult {
                        total_lines: assembled.total_lines,
                        primary: result,
                        related,
                        strategy,
                    });
                }
                Err(_) => enriched.push(crate::context_search::EnrichedResult {
                    total_lines: result.chunk.line_count(),
                    primary: result,
                    related: vec![],
                    strategy,
                }),
            }
        }

        Ok(enriched)
    }

    #[must_use]
    pub const fn hybrid(&self) -> &MultiModelHybridSearch {
        &self.hybrid
    }

    pub const fn hybrid_mut(&mut self) -> &mut MultiModelHybridSearch {
        &mut self.hybrid
    }
}

fn semantic_disable_reason(err: &SearchError) -> Option<String> {
    match err {
        SearchError::VectorStoreError(context_vector_store::VectorStoreError::EmbeddingError(
            message,
        )) => Some(message.clone()),
        _ => None,
    }
}

fn collect_chunks(store: &VectorIndex) -> (Vec<CodeChunk>, HashMap<String, usize>) {
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

fn collect_chunks_from_corpus(
    corpus: &ChunkCorpus,
    store: &VectorIndex,
) -> (Vec<CodeChunk>, HashMap<String, usize>) {
    let mut chunks = Vec::new();
    let mut lookup = HashMap::new();
    for id in store.chunk_ids() {
        let Some(chunk) = corpus.get_chunk(&id) else {
            continue;
        };
        lookup.insert(id, chunks.len());
        chunks.push(chunk.clone());
    }
    (chunks, lookup)
}

fn candidate_pool(limit: usize, multiplier: usize) -> usize {
    let limit = limit.max(1);
    let multiplier = multiplier.max(1);
    limit * multiplier
}

fn has_file_extension(token: &str) -> bool {
    let token = token.trim();
    let Some((_, ext)) = token.rsplit_once('.') else {
        return false;
    };
    if ext.is_empty() || ext.len() > 6 {
        return false;
    }
    ext.chars().all(|c| c.is_ascii_alphanumeric())
}

fn normalize_path_query(input: &str) -> String {
    let input = input.trim().trim_matches(|c| c == '"' || c == '\'');
    input
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_ascii_lowercase()
}

fn filter_fuzzy(scores: Vec<(usize, f32)>, rejected: &[bool], min_score: f32) -> Vec<(usize, f32)> {
    scores
        .into_iter()
        .filter(|(idx, score)| *score >= min_score && !rejected.get(*idx).copied().unwrap_or(false))
        .collect()
}

/// Return a list of unique indices ordered by decreasing fused RRF score.
fn fuse_rrf(rankings: &[Vec<usize>], k: f32) -> Vec<usize> {
    let mut scores: HashMap<usize, f32> = HashMap::new();

    for ranking in rankings {
        for (rank, idx) in ranking.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let score = 1.0 / (k + rank as f32 + 1.0);
            *scores.entry(*idx).or_insert(0.0) += score;
        }
    }

    let mut fused: Vec<(usize, f32)> = scores.into_iter().collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    fused.into_iter().map(|(idx, _)| idx).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_code_chunker::{ChunkMetadata, ChunkType, CodeChunk};
    use context_vector_store::StoredChunk;
    use serde::Serialize;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[derive(Serialize)]
    struct PersistedStore {
        chunks: HashMap<String, StoredChunk>,
        id_map: HashMap<usize, String>,
        next_id: usize,
        dimension: usize,
    }

    fn chunk(path: &str, content: &str) -> CodeChunk {
        CodeChunk::new(
            path.to_string(),
            1,
            2,
            content.to_string(),
            ChunkMetadata::default(),
        )
    }

    fn chunk_with_symbol(path: &str, symbol: &str, content: &str) -> CodeChunk {
        CodeChunk::new(
            path.to_string(),
            1,
            2,
            content.to_string(),
            ChunkMetadata::default()
                .chunk_type(ChunkType::Function)
                .symbol_name(symbol),
        )
    }

    async fn write_index(
        dir: &TempDir,
        registry: &ModelRegistry,
        model_id: &str,
        name: &str,
        chunks: Vec<CodeChunk>,
    ) -> Result<VectorIndex> {
        let texts: Vec<&str> = chunks.iter().map(|c| c.content.as_str()).collect();
        let vectors = registry.embed_batch(model_id, texts).await?;

        let mut map = HashMap::new();
        let mut id_map = HashMap::new();
        for (numeric_id, (chunk, vector)) in chunks.into_iter().zip(vectors).enumerate() {
            let id = format!(
                "{}:{}:{}",
                chunk.file_path, chunk.start_line, chunk.end_line
            );
            id_map.insert(numeric_id, id.clone());
            map.insert(
                id.clone(),
                StoredChunk {
                    chunk,
                    vector: std::sync::Arc::new(vector),
                    id,
                    doc_hash: 0,
                },
            );
        }

        let dimension = registry.dimension(model_id)?;
        let persisted = PersistedStore {
            chunks: map,
            id_map,
            next_id: 2,
            dimension,
        };

        let path = dir.path().join(name);
        let data = serde_json::to_vec_pretty(&persisted)
            .map_err(|e| SearchError::Other(format!("serialize index failed: {e}")))?;
        tokio::fs::write(&path, data)
            .await
            .map_err(|e| SearchError::Other(format!("write index failed: {e}")))?;
        VectorIndex::load(&path).await.map_err(Into::into)
    }

    #[tokio::test]
    async fn multi_model_search_prefers_exact_stub_match() {
        let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("models");
        let registry = ModelRegistry::new_stub(model_dir).unwrap();

        let tmp = TempDir::new().unwrap();
        let chunks = vec![chunk("a.rs", "alpha"), chunk("b.rs", "beta")];

        let idx_small = write_index(&tmp, &registry, "bge-small", "small.json", chunks.clone())
            .await
            .unwrap();
        let idx_base = write_index(&tmp, &registry, "bge-base", "base.json", chunks)
            .await
            .unwrap();

        let sources = vec![
            ("bge-small".to_string(), idx_small),
            ("bge-base".to_string(), idx_base),
        ];
        let profile = SearchProfile::general();
        let mut search = MultiModelHybridSearch::new(sources, profile, registry).unwrap();

        let results = search.search("alpha", 3).await.unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].id, "a.rs:1:2");
    }

    #[tokio::test]
    async fn path_queries_return_direct_file_hits() {
        let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("models");
        let registry = ModelRegistry::new_stub(model_dir).unwrap();

        let tmp = TempDir::new().unwrap();
        let chunks = vec![
            chunk("crates/vector-store/src/corpus.rs", "corpus impl"),
            chunk(
                "crates/cli/Cargo.toml",
                "context-vector-store = { path = \"../vector-store\" }",
            ),
        ];

        let idx_small = write_index(&tmp, &registry, "bge-small", "small.json", chunks.clone())
            .await
            .unwrap();
        let idx_base = write_index(&tmp, &registry, "bge-base", "base.json", chunks)
            .await
            .unwrap();

        let sources = vec![
            ("bge-small".to_string(), idx_small),
            ("bge-base".to_string(), idx_base),
        ];
        let profile = SearchProfile::general();
        let mut search = MultiModelHybridSearch::new(sources, profile, registry).unwrap();

        let results = search
            .search("crates/vector-store/src/corpus.rs", 10)
            .await
            .unwrap();
        assert!(!results.is_empty());
        assert_eq!(
            results[0].chunk.file_path,
            "crates/vector-store/src/corpus.rs"
        );
        assert!(results
            .iter()
            .all(|r| r.chunk.file_path == "crates/vector-store/src/corpus.rs"));
    }

    #[tokio::test]
    async fn identifier_queries_return_direct_symbol_hits() {
        let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("models");
        let registry = ModelRegistry::new_stub(model_dir).unwrap();

        let tmp = TempDir::new().unwrap();
        let chunks = vec![
            chunk_with_symbol(
                "crates/mcp-server/tests/mcp_smoke.rs",
                "locate_context_finder_mcp_bin",
                "fn locate_context_finder_mcp_bin() {}",
            ),
            chunk_with_symbol("src/lib.rs", "other", "fn other() {}"),
        ];

        let idx_small = write_index(&tmp, &registry, "bge-small", "small.json", chunks.clone())
            .await
            .unwrap();
        let idx_base = write_index(&tmp, &registry, "bge-base", "base.json", chunks)
            .await
            .unwrap();

        let sources = vec![
            ("bge-small".to_string(), idx_small),
            ("bge-base".to_string(), idx_base),
        ];
        let profile = SearchProfile::general();
        let mut search = MultiModelHybridSearch::new(sources, profile, registry).unwrap();

        let results = search
            .search("locate_context_finder_mcp_bin", 10)
            .await
            .unwrap();

        assert!(!results.is_empty());
        assert_eq!(
            results[0].chunk.file_path,
            "crates/mcp-server/tests/mcp_smoke.rs"
        );
        assert!(results.iter().all(|r| {
            r.chunk
                .metadata
                .symbol_name
                .as_deref()
                .is_some_and(|s| s == "locate_context_finder_mcp_bin")
        }));

        // Mixed queries that include an identifier should still return direct symbol hits
        // (agent UX: "IDENTIFIER + clarification words").
        let mixed = search
            .search("locate_context_finder_mcp_bin drift validation", 10)
            .await
            .unwrap();
        assert!(!mixed.is_empty());
        assert_eq!(
            mixed[0].chunk.file_path,
            "crates/mcp-server/tests/mcp_smoke.rs"
        );

        // Conceptual query with embedded identifier should also anchor to the symbol when possible.
        let conceptual = search
            .search("how does locate_context_finder_mcp_bin work", 10)
            .await
            .unwrap();
        assert!(!conceptual.is_empty());
        assert_eq!(
            conceptual[0].chunk.file_path,
            "crates/mcp-server/tests/mcp_smoke.rs"
        );
    }

    #[tokio::test]
    async fn embedding_errors_degrade_to_fuzzy_only_search() {
        let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("models");
        let registry = ModelRegistry::new_stub(model_dir).unwrap();

        let tmp = TempDir::new().unwrap();
        let chunks = vec![chunk("a.rs", "alpha"), chunk("b.rs", "beta")];

        let idx_small = write_index(&tmp, &registry, "bge-small", "small.json", chunks.clone())
            .await
            .unwrap();

        // Purposely use a model id that the registry does not know about. Semantic search should
        // fail with an embedding error, and the engine must degrade to fuzzy-only results rather
        // than failing the whole query.
        let sources = vec![("unknown-model".to_string(), idx_small)];
        let profile = SearchProfile::general();
        let mut search = MultiModelHybridSearch::new(sources, profile, registry).unwrap();

        assert!(search.semantic_disabled_reason().is_none());

        let results = search.search("alpha", 3).await.unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].id, "a.rs:1:2");

        let disabled = search.semantic_disabled_reason();
        assert!(disabled.is_some());
        assert!(disabled
            .unwrap()
            .contains("Unknown embedding model id 'unknown-model'"));

        // Once semantic search has been disabled, subsequent searches should keep working.
        let results = search.search("beta", 3).await.unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].id, "b.rs:1:2");
    }
}
