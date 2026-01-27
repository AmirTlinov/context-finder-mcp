use crate::embedding_cache::EmbeddingCache;
use crate::embeddings::EmbeddingModel;
use crate::error::Result;
use crate::hnsw_index::HnswIndex;
use crate::paths::{default_context_dir_rel, find_context_dir_from_path};
use crate::templates::{DocumentTemplates, EmbeddingTemplates};
use crate::types::{SearchResult, StoredChunk};
use crate::ChunkCorpus;
use context_code_chunker::CodeChunk;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

pub struct VectorStore {
    chunks: HashMap<String, StoredChunk>,
    index: HnswIndex,
    embedder: EmbeddingModel,
    path: std::path::PathBuf,
    next_id: usize,
    id_map: HashMap<usize, String>, // numeric_id -> string_id mapping
    reverse_id_map: HashMap<String, usize>, // string_id -> numeric_id mapping
    model_id: String,
    embedding_mode: String,
    dimension: usize,
    templates: EmbeddingTemplates,
    embedding_cache: EmbeddingCache,
}

/// Read-only view of a persisted `VectorStore` that can perform similarity search given query
/// vectors, without requiring an embedding model to be available at runtime.
pub struct VectorIndex {
    chunks: HashMap<String, StoredChunk>,
    index: HnswIndex,
    id_map: HashMap<usize, String>,
    dimension: usize,
}

const VECTOR_STORE_SCHEMA_VERSION: u32 = 3;

#[derive(Serialize, Deserialize)]
struct PersistedVectorStoreV3 {
    schema_version: u32,
    dimension: usize,
    next_id: usize,
    id_map: BTreeMap<usize, String>,
    vectors: BTreeMap<String, PersistedVectorEntryV3>,
}

#[derive(Serialize, Deserialize)]
struct PersistedVectorEntryV3 {
    vector: Arc<Vec<f32>>,
    #[serde(default)]
    doc_hash: u64,
}

struct PersistedStoreData {
    chunks: HashMap<String, StoredChunk>,
    id_map_raw: HashMap<usize, String>,
    stored_next_id: usize,
    stored_dimension: usize,
}

fn normalize_arc_vector(vector: &mut Arc<Vec<f32>>) {
    let arc = std::mem::replace(vector, Arc::new(Vec::new()));
    let mut owned = match Arc::try_unwrap(arc) {
        Ok(vec) => vec,
        Err(arc) => (*arc).clone(),
    };
    crate::hnsw_index::normalize_in_place(&mut owned);
    *vector = Arc::new(owned);
}

impl VectorIndex {
    pub async fn load(path: &Path) -> Result<Self> {
        log::info!("Loading VectorIndex from {}", path.display());
        let data = tokio::fs::read_to_string(path).await?;
        let save_data: serde_json::Value = serde_json::from_str(&data)?;

        let schema_version = save_data
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1);

        let (mut chunks, id_map_raw, mut vectors, dimension) =
            if schema_version == u64::from(VECTOR_STORE_SCHEMA_VERSION) {
                let persisted: PersistedVectorStoreV3 = serde_json::from_value(save_data)?;
                (
                    HashMap::new(),
                    persisted.id_map.into_iter().collect(),
                    persisted
                        .vectors
                        .into_iter()
                        .map(|(id, entry)| (id, entry.vector))
                        .collect::<HashMap<String, Arc<Vec<f32>>>>(),
                    persisted.dimension,
                )
            } else if schema_version == 1 {
                let chunks: HashMap<String, StoredChunk> =
                    serde_json::from_value(save_data["chunks"].clone())?;
                let id_map_raw: HashMap<usize, String> =
                    serde_json::from_value(save_data["id_map"].clone())?;
                let dimension: usize = save_data
                    .get("dimension")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .unwrap_or(384);
                (chunks, id_map_raw, HashMap::new(), dimension)
            } else {
                return Err(crate::VectorStoreError::EmbeddingError(format!(
                    "Unsupported VectorIndex schema_version {schema_version}"
                )));
            };

        let mut id_pairs: Vec<(usize, String)> = id_map_raw.into_iter().collect();
        id_pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let mut id_map: HashMap<usize, String> = HashMap::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut dupes = 0usize;
        for (numeric_id, string_id) in id_pairs {
            if !seen.insert(string_id.clone()) {
                dupes += 1;
                continue;
            }
            id_map.insert(numeric_id, string_id);
        }
        if dupes > 0 {
            log::warn!(
                "Detected {dupes} duplicate id_map entries while loading VectorIndex; repaired by deduplicating on load"
            );
        }

        for stored in chunks.values_mut() {
            normalize_arc_vector(&mut stored.vector);
        }
        for vector in vectors.values_mut() {
            normalize_arc_vector(vector);
        }

        let mut index = HnswIndex::new(dimension);
        let mut numeric_ids: Vec<usize> = id_map.keys().copied().collect();
        numeric_ids.sort();
        for numeric_id in numeric_ids {
            let Some(string_id) = id_map.get(&numeric_id) else {
                continue;
            };
            if let Some(stored) = chunks.get(string_id) {
                index.add_shared(numeric_id, stored.vector.clone())?;
            } else if let Some(vector) = vectors.get(string_id) {
                index.add_shared(numeric_id, vector.clone())?;
            }
        }

        Ok(Self {
            chunks,
            index,
            id_map,
            dimension,
        })
    }

    #[must_use]
    pub const fn dimension(&self) -> usize {
        self.dimension
    }

    #[must_use]
    pub fn get_chunk(&self, id: &str) -> Option<&StoredChunk> {
        self.chunks.get(id)
    }

    #[must_use]
    pub fn chunk_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = if self.chunks.is_empty() {
            self.id_map.values().cloned().collect()
        } else {
            self.chunks.keys().cloned().collect()
        };
        ids.sort();
        ids.dedup();
        ids
    }

    pub fn search_by_vector(
        &self,
        query_vector: &[f32],
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let neighbors = self.index.search(query_vector, limit)?;

        let mut results = Vec::with_capacity(neighbors.len());
        for (chunk_id, score) in neighbors {
            if let Some(stored) = self.find_chunk_by_numeric_id(chunk_id) {
                results.push(SearchResult {
                    chunk: stored.chunk.clone(),
                    score,
                    id: stored.id.clone(),
                });
            }
        }

        Ok(results)
    }

    pub fn search_ids_by_vector(
        &self,
        query_vector: &[f32],
        limit: usize,
    ) -> Result<Vec<(String, f32)>> {
        let neighbors = self.index.search(query_vector, limit)?;
        let mut hits = Vec::with_capacity(neighbors.len());
        for (numeric_id, score) in neighbors {
            if let Some(chunk_id) = self.id_map.get(&numeric_id) {
                hits.push((chunk_id.clone(), score));
            }
        }
        Ok(hits)
    }

    fn find_chunk_by_numeric_id(&self, id: usize) -> Option<&StoredChunk> {
        self.id_map
            .get(&id)
            .and_then(|string_id| self.chunks.get(string_id))
    }
}

impl VectorStore {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        Self::new_with_templates(path, EmbeddingTemplates::default())
    }

    pub fn new_with_templates(
        path: impl AsRef<Path>,
        templates: EmbeddingTemplates,
    ) -> Result<Self> {
        let model_id = crate::current_model_id().unwrap_or_else(|_| "bge-small".to_string());
        Self::new_with_templates_for_model(path, &model_id, templates)
    }

    pub fn new_for_model(path: impl AsRef<Path>, model_id: &str) -> Result<Self> {
        Self::new_with_templates_for_model(path, model_id, EmbeddingTemplates::default())
    }

    pub fn new_with_templates_for_model(
        path: impl AsRef<Path>,
        model_id: &str,
        templates: EmbeddingTemplates,
    ) -> Result<Self> {
        log::info!("Initializing VectorStore at {}", path.as_ref().display());
        templates.validate()?;
        let embedder = EmbeddingModel::new_for_model(model_id)?;
        let embedding_mode = crate::embeddings::current_embedding_mode_id()?.to_string();
        let dimension = embedder.dimension();
        let index = HnswIndex::new(dimension);

        Ok(Self {
            chunks: HashMap::new(),
            index,
            embedder,
            path: path.as_ref().to_path_buf(),
            next_id: 0,
            id_map: HashMap::new(),
            reverse_id_map: HashMap::new(),
            model_id: model_id.to_string(),
            embedding_mode,
            dimension,
            templates,
            embedding_cache: EmbeddingCache::for_store_path(path.as_ref()),
        })
    }

    /// Add chunks with batch embedding for efficiency
    pub async fn add_chunks(&mut self, chunks: Vec<CodeChunk>) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }

        log::info!("Adding {} chunks to store", chunks.len());

        // Render embedding input with templates (deterministic + bounded)
        let mut rendered = Vec::with_capacity(chunks.len());
        let mut doc_hashes = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            let doc = self.templates.render_doc_chunk(chunk)?;
            doc_hashes.push(fnv1a64(doc.as_bytes()));
            rendered.push(doc);
        }
        let vectors = self.embed_rendered_docs(&rendered, &doc_hashes).await?;

        // Store chunks with their vectors
        for ((chunk, vector), doc_hash) in chunks
            .into_iter()
            .zip(vectors.into_iter())
            .zip(doc_hashes.into_iter())
        {
            let id = format!(
                "{}:{}:{}",
                chunk.file_path, chunk.start_line, chunk.end_line
            );
            let numeric_id = if let Some(existing) = self.reverse_id_map.get(&id).copied() {
                existing
            } else {
                let numeric_id = self.next_id;
                self.next_id += 1;
                self.id_map.insert(numeric_id, id.clone());
                self.reverse_id_map.insert(id.clone(), numeric_id);
                numeric_id
            };

            let mut vector = vector;
            crate::hnsw_index::normalize_in_place(&mut vector);
            let vector = Arc::new(vector);

            // Add to HNSW index (shared vector, no duplication)
            self.index.add_shared(numeric_id, vector.clone())?;

            // Add to id mapping
            self.id_map.insert(numeric_id, id.clone());

            let stored = StoredChunk {
                chunk,
                vector,
                id: id.clone(),
                doc_hash,
            };
            self.chunks.insert(id, stored);
        }

        log::info!("Successfully added chunks. Total: {}", self.chunks.len());
        Ok(())
    }

    async fn embed_rendered_docs(
        &self,
        rendered: &[String],
        doc_hashes: &[u64],
    ) -> Result<Vec<Vec<f32>>> {
        if rendered.is_empty() {
            return Ok(vec![]);
        }

        let template_hash = self.templates.doc_template_hash();
        let mut vectors: Vec<Option<Vec<f32>>> = vec![None; rendered.len()];
        let mut miss_indices = Vec::new();
        let mut miss_texts = Vec::new();

        for (idx, (doc, doc_hash)) in rendered.iter().zip(doc_hashes.iter()).enumerate() {
            if let Some(vec) = self
                .embedding_cache
                .get_vector(
                    &self.embedding_mode,
                    &self.model_id,
                    template_hash,
                    *doc_hash,
                    self.dimension,
                )
                .await
            {
                vectors[idx] = Some(vec);
            } else {
                miss_indices.push(idx);
                miss_texts.push(doc.as_str());
            }
        }

        if !miss_indices.is_empty() {
            let embedded = self.embedder.embed_batch(miss_texts).await?;
            for (idx, vector) in miss_indices.into_iter().zip(embedded.into_iter()) {
                let doc_hash = doc_hashes[idx];
                if let Err(err) = self
                    .embedding_cache
                    .put_vector(
                        &self.embedding_mode,
                        &self.model_id,
                        template_hash,
                        doc_hash,
                        &vector,
                    )
                    .await
                {
                    log::warn!("Failed to persist embedding cache entry: {err:#}");
                }
                vectors[idx] = Some(vector);
            }
        }

        let mut out = Vec::with_capacity(vectors.len());
        for vec in vectors {
            out.push(vec.ok_or_else(|| {
                crate::VectorStoreError::EmbeddingError(
                    "Missing embedding vector after cache/embed".to_string(),
                )
            })?);
        }
        Ok(out)
    }

    /// Search for similar chunks using semantic similarity
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        self.search_with_embedding_text(query, limit).await
    }

    /// Search for similar chunks using semantic similarity, where `embedding_text` has already been
    /// prepared (e.g., templated/prompted) by the caller.
    pub async fn search_with_embedding_text(
        &self,
        embedding_text: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        log::debug!("Searching semantic index (limit: {limit})");

        // Embed query
        let query_vector = self.embedder.embed(embedding_text).await?;

        // Search HNSW index
        let neighbors = self.index.search(&query_vector, limit)?;

        // Convert to SearchResult
        let mut results = Vec::new();
        for (chunk_id, score) in neighbors {
            // Find chunk by numeric id
            if let Some(stored) = self.find_chunk_by_numeric_id(chunk_id) {
                results.push(SearchResult {
                    chunk: stored.chunk.clone(),
                    score,
                    id: stored.id.clone(),
                });
            }
        }

        log::debug!("Found {} results", results.len());
        Ok(results)
    }

    /// Batch search for multiple queries (more efficient than sequential searches)
    /// Returns results for each query in the same order
    pub async fn search_batch(
        &self,
        queries: &[&str],
        limit: usize,
    ) -> Result<Vec<Vec<SearchResult>>> {
        self.search_batch_with_embedding_texts(queries, limit).await
    }

    /// Batch search where each query has already been prepared (e.g., templated/prompted) by the
    /// caller.
    pub async fn search_batch_with_embedding_texts(
        &self,
        embedding_texts: &[&str],
        limit: usize,
    ) -> Result<Vec<Vec<SearchResult>>> {
        if embedding_texts.is_empty() {
            return Ok(vec![]);
        }

        log::debug!(
            "Batch searching {} queries (limit: {})",
            embedding_texts.len(),
            limit
        );

        // Batch embed all queries (much more efficient)
        let query_vectors = self.embedder.embed_batch(embedding_texts.to_vec()).await?;

        // Search for each query vector
        let mut all_results = Vec::with_capacity(embedding_texts.len());
        for (i, query_vector) in query_vectors.iter().enumerate() {
            log::debug!("Searching query {}/{}", i + 1, embedding_texts.len());

            let neighbors = self.index.search(query_vector, limit)?;

            let mut results = Vec::new();
            for (chunk_id, score) in neighbors {
                if let Some(stored) = self.find_chunk_by_numeric_id(chunk_id) {
                    results.push(SearchResult {
                        chunk: stored.chunk.clone(),
                        score,
                        id: stored.id.clone(),
                    });
                }
            }

            all_results.push(results);
        }

        log::debug!(
            "Batch search completed: {} queries processed",
            embedding_texts.len()
        );
        Ok(all_results)
    }

    /// Find chunk by numeric ID using `id_map`
    fn find_chunk_by_numeric_id(&self, id: usize) -> Option<&StoredChunk> {
        self.id_map
            .get(&id)
            .and_then(|string_id| self.chunks.get(string_id))
    }

    /// Get chunk by string ID
    #[must_use]
    pub fn get_chunk(&self, id: &str) -> Option<&StoredChunk> {
        self.chunks.get(id)
    }

    /// Get all chunk IDs
    #[must_use]
    pub fn chunk_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.chunks.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Get total number of chunks
    #[must_use]
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// Check if store is empty
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Remove all chunks belonging to a single file path (relative path, e.g. `src/lib.rs`).
    /// Returns the number of removed chunks.
    pub fn remove_chunks_for_file(&mut self, file_path: &str) -> usize {
        let ids: Vec<String> = self
            .chunks
            .iter()
            .filter_map(|(id, stored)| {
                if stored.chunk.file_path == file_path {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();

        let mut removed = 0usize;
        for id in ids {
            if self.remove_chunk_id(&id) {
                removed += 1;
            }
        }
        removed
    }

    /// Drop chunks whose `chunk.file_path` is not present in `live_files`.
    /// Returns the number of removed chunks.
    pub fn purge_missing_files(&mut self, live_files: &HashSet<String>) -> usize {
        let ids: Vec<String> = self
            .chunks
            .iter()
            .filter_map(|(id, stored)| {
                if live_files.contains(&stored.chunk.file_path) {
                    None
                } else {
                    Some(id.clone())
                }
            })
            .collect();

        let mut removed = 0usize;
        for id in ids {
            if self.remove_chunk_id(&id) {
                removed += 1;
            }
        }
        removed
    }

    /// Remove a single chunk by its string id (`<file>:<start>:<end>`).
    ///
    /// This is primarily used for index self-healing when detecting corpus/index drift.
    pub fn remove_chunk_by_id(&mut self, id: &str) -> bool {
        self.remove_chunk_id(id)
    }

    fn remove_chunk_id(&mut self, id: &str) -> bool {
        if self.chunks.remove(id).is_none() {
            return false;
        }

        if let Some(numeric_id) = self.reverse_id_map.remove(id) {
            self.id_map.remove(&numeric_id);
            self.index.remove(numeric_id);
        }
        true
    }

    /// Save store to disk
    pub async fn save(&self) -> Result<()> {
        self.save_impl(&self.path, SaveMode::Normal).await
    }

    /// Save store to an alternate location (index + meta), without mutating or pruning caches.
    ///
    /// This is intended for staging/transactional writes in higher-level indexers.
    pub async fn save_to_path(&self, path: impl AsRef<Path>) -> Result<()> {
        self.save_impl(path.as_ref(), SaveMode::Staged).await
    }

    async fn save_impl(&self, path: &Path, mode: SaveMode) -> Result<()> {
        log::info!("Saving VectorStore to {}", path.display());

        let mut vectors: BTreeMap<String, PersistedVectorEntryV3> = BTreeMap::new();
        for (id, stored) in &self.chunks {
            vectors.insert(
                id.clone(),
                PersistedVectorEntryV3 {
                    vector: stored.vector.clone(),
                    doc_hash: stored.doc_hash,
                },
            );
        }

        let mut id_map: BTreeMap<usize, String> = BTreeMap::new();
        for (numeric_id, chunk_id) in &self.id_map {
            id_map.insert(*numeric_id, chunk_id.clone());
        }

        let persisted = PersistedVectorStoreV3 {
            schema_version: VECTOR_STORE_SCHEMA_VERSION,
            dimension: self.dimension,
            next_id: self.next_id,
            id_map,
            vectors,
        };

        let data = serde_json::to_vec_pretty(&persisted)?;
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, data).await?;
        tokio::fs::rename(&tmp, path).await?;
        self.save_meta_for_store_path(path).await?;
        if mode == SaveMode::Normal {
            if let Some(max_bytes) = embed_cache_max_bytes_from_env() {
                self.embedding_cache
                    .prune_model_dir(&self.embedding_mode, &self.model_id, max_bytes)
                    .await;
            }
        }
        log::info!("VectorStore saved successfully");
        Ok(())
    }

    /// Load store from disk
    pub async fn load(path: &Path) -> Result<Self> {
        log::info!("Loading VectorStore from {}", path.display());
        let cached_meta = load_meta_info(path).await;
        let templates = cached_meta
            .as_ref()
            .map(|m| m.templates.clone())
            .unwrap_or_default();
        let model_id = crate::current_model_id().unwrap_or_else(|_| "bge-small".to_string());
        Self::load_with_templates_for_model(path, templates, &model_id).await
    }

    pub async fn load_with_templates(path: &Path, templates: EmbeddingTemplates) -> Result<Self> {
        let model_id = crate::current_model_id().unwrap_or_else(|_| "bge-small".to_string());
        Self::load_with_templates_for_model(path, templates, &model_id).await
    }

    pub async fn load_for_model(path: &Path, model_id: &str) -> Result<Self> {
        log::info!(
            "Loading VectorStore from {} (model_id={})",
            path.display(),
            model_id
        );
        let cached_meta = load_meta_info(path).await;
        let templates = cached_meta
            .as_ref()
            .map(|m| m.templates.clone())
            .unwrap_or_default();
        Self::load_with_templates_for_model(path, templates, model_id).await
    }

    pub async fn load_with_templates_for_model(
        path: &Path,
        templates: EmbeddingTemplates,
        model_id: &str,
    ) -> Result<Self> {
        let cached_meta = load_meta_info(path).await;
        let data = tokio::fs::read_to_string(path).await?;
        let save_data: serde_json::Value = serde_json::from_str(&data)?;

        let schema_version = save_data
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1);

        let PersistedStoreData {
            chunks,
            id_map_raw,
            stored_next_id,
            stored_dimension,
        } = Self::load_persisted_store_data(path, schema_version, save_data).await?;

        let embedder = EmbeddingModel::new_for_model(model_id)?;
        let embedding_mode = crate::embeddings::current_embedding_mode_id()?.to_string();
        let dimension = embedder.dimension();
        let templates = {
            templates.validate()?;
            templates
        };

        if let Some(meta) = cached_meta.as_ref() {
            if meta.dimension != stored_dimension {
                log::debug!(
                    "Meta dimension {} differs from stored {}, will re-embed",
                    meta.dimension,
                    stored_dimension
                );
            }
        }

        let (id_map, reverse_id_map) = Self::repair_id_maps(&chunks, id_map_raw);

        let next_id: usize = id_map.keys().max().copied().map_or(stored_next_id, |id| {
            id.saturating_add(1).max(stored_next_id)
        });

        let mut index = HnswIndex::new(dimension);

        // Rebuild index using id_map
        let mut numeric_ids: Vec<usize> = id_map.keys().copied().collect();
        numeric_ids.sort();
        for numeric_id in numeric_ids {
            let Some(string_id) = id_map.get(&numeric_id) else {
                continue;
            };
            if let Some(stored) = chunks.get(string_id) {
                index.add_shared(numeric_id, stored.vector.clone())?;
            }
        }

        log::info!("Loaded {} chunks", chunks.len());

        let mut store = Self {
            chunks,
            index,
            embedder,
            path: path.to_path_buf(),
            next_id,
            id_map,
            reverse_id_map,
            model_id: model_id.to_string(),
            embedding_mode,
            dimension,
            templates,
            embedding_cache: EmbeddingCache::for_store_path(path),
        };

        store
            .reconcile_with_persisted_state(stored_dimension, cached_meta.as_ref())
            .await?;

        Ok(store)
    }

    async fn load_persisted_store_data(
        path: &Path,
        schema_version: u64,
        save_data: serde_json::Value,
    ) -> Result<PersistedStoreData> {
        if schema_version == u64::from(VECTOR_STORE_SCHEMA_VERSION) {
            let persisted: PersistedVectorStoreV3 = serde_json::from_value(save_data)?;
            Self::load_v3_store_data(path, persisted).await
        } else if schema_version == 1 {
            Self::load_v1_store_data(&save_data)
        } else {
            Err(crate::VectorStoreError::EmbeddingError(format!(
                "Unsupported VectorStore schema_version {schema_version}"
            )))
        }
    }

    async fn load_v3_store_data(
        path: &Path,
        persisted: PersistedVectorStoreV3,
    ) -> Result<PersistedStoreData> {
        let corpus_path = corpus_path_for_store_path(path);
        let corpus = ChunkCorpus::load(&corpus_path).await.map_err(|err| {
            crate::VectorStoreError::EmbeddingError(format!(
                "Failed to load chunk corpus at {}: {err}",
                corpus_path.display()
            ))
        })?;

        let mut chunks: HashMap<String, StoredChunk> =
            HashMap::with_capacity(persisted.vectors.len());
        let mut missing_chunks = 0usize;
        let mut missing_examples: Vec<String> = Vec::new();
        for (id, entry) in persisted.vectors {
            let Some(chunk) = corpus.get_chunk(&id) else {
                missing_chunks += 1;
                if missing_examples.len() < 3 {
                    missing_examples.push(id.clone());
                }
                continue;
            };
            chunks.insert(
                id.clone(),
                StoredChunk {
                    chunk: chunk.clone(),
                    vector: entry.vector,
                    id,
                    doc_hash: entry.doc_hash,
                },
            );
        }
        if missing_chunks > 0 {
            let examples = if missing_examples.is_empty() {
                String::new()
            } else {
                format!(" (examples: {})", missing_examples.join(", "))
            };
            log::warn!(
                "VectorStore {}: dropped {missing_chunks} stale vectors because their chunks are missing in corpus {}{examples}",
                path.display(),
                corpus_path.display()
            );
        }

        for stored in chunks.values_mut() {
            normalize_arc_vector(&mut stored.vector);
        }

        Ok(PersistedStoreData {
            chunks,
            id_map_raw: persisted.id_map.into_iter().collect(),
            stored_next_id: persisted.next_id,
            stored_dimension: persisted.dimension,
        })
    }

    fn load_v1_store_data(save_data: &serde_json::Value) -> Result<PersistedStoreData> {
        let mut chunks: HashMap<String, StoredChunk> =
            serde_json::from_value(save_data["chunks"].clone())?;
        let id_map_raw: HashMap<usize, String> =
            serde_json::from_value(save_data["id_map"].clone())?;
        let stored_next_id: usize = save_data["next_id"]
            .as_u64()
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(0);
        let stored_dimension: usize = save_data
            .get("dimension")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(384);

        for stored in chunks.values_mut() {
            normalize_arc_vector(&mut stored.vector);
        }

        Ok(PersistedStoreData {
            chunks,
            id_map_raw,
            stored_next_id,
            stored_dimension,
        })
    }

    fn repair_id_maps(
        chunks: &HashMap<String, StoredChunk>,
        id_map_raw: HashMap<usize, String>,
    ) -> (HashMap<usize, String>, HashMap<String, usize>) {
        let mut id_pairs: Vec<(usize, String)> = id_map_raw.into_iter().collect();
        id_pairs.sort_by(|a, b| a.0.cmp(&b.0));

        let mut id_map: HashMap<usize, String> = HashMap::new();
        let mut reverse_id_map: HashMap<String, usize> = HashMap::new();
        let mut dupes = 0usize;
        let mut missing = 0usize;
        for (numeric_id, string_id) in id_pairs {
            if !chunks.contains_key(&string_id) {
                missing += 1;
                continue;
            }
            if reverse_id_map.contains_key(&string_id) {
                dupes += 1;
                continue;
            }
            reverse_id_map.insert(string_id.clone(), numeric_id);
            id_map.insert(numeric_id, string_id);
        }
        if dupes > 0 {
            log::warn!(
                "Detected {dupes} duplicate id_map entries; repaired by deduplicating on load"
            );
        }
        if missing > 0 {
            log::warn!(
                "Detected {missing} id_map entries pointing to missing chunks; repaired by dropping them on load"
            );
        }

        (id_map, reverse_id_map)
    }

    async fn reconcile_with_persisted_state(
        &mut self,
        stored_dimension: usize,
        cached_meta: Option<&StoreMetaInfo>,
    ) -> Result<()> {
        if self.dimension != stored_dimension {
            log::warn!(
                "Embedding dimension changed ({} → {}), re-embedding stored vectors",
                stored_dimension,
                self.dimension
            );
            self.reembed_all_chunks().await?;
            self.save().await?;
            return Ok(());
        }

        let Some(meta) = cached_meta else {
            return Ok(());
        };

        if meta.embedding_mode != self.embedding_mode {
            log::warn!(
                "Embedding mode changed ({} → {}), re-embedding stored vectors",
                meta.embedding_mode,
                self.embedding_mode
            );
            self.reembed_all_chunks().await?;
            self.save().await?;
            return Ok(());
        }

        if meta.doc_template_hash != self.templates.doc_template_hash() {
            log::warn!(
                "Embedding doc template changed ({} → {}), re-embedding stored vectors",
                meta.doc_template_hash,
                self.templates.doc_template_hash()
            );
            self.reembed_all_chunks().await?;
            self.save().await?;
        }

        Ok(())
    }

    async fn reembed_all_chunks(&mut self) -> Result<()> {
        let mut entries: Vec<(String, CodeChunk)> = self
            .chunks
            .iter()
            .map(|(id, stored)| (id.clone(), stored.chunk.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut rendered = Vec::with_capacity(entries.len());
        let mut doc_hashes = Vec::with_capacity(entries.len());
        for (_, chunk) in &entries {
            let doc = self.templates.render_doc_chunk(chunk)?;
            doc_hashes.push(fnv1a64(doc.as_bytes()));
            rendered.push(doc);
        }
        let vectors = self.embed_rendered_docs(&rendered, &doc_hashes).await?;

        self.index = HnswIndex::new(self.dimension);
        self.id_map.clear();
        self.reverse_id_map.clear();
        self.next_id = 0;

        let mut new_chunks = HashMap::new();
        for (((id, chunk), vector), doc_hash) in entries
            .into_iter()
            .zip(vectors.into_iter())
            .zip(doc_hashes.into_iter())
        {
            let numeric_id = self.next_id;
            self.next_id += 1;
            let mut vector = vector;
            crate::hnsw_index::normalize_in_place(&mut vector);
            let vector = Arc::new(vector);
            self.index.add_shared(numeric_id, vector.clone())?;
            self.id_map.insert(numeric_id, id.clone());
            self.reverse_id_map.insert(id.clone(), numeric_id);
            new_chunks.insert(
                id.clone(),
                StoredChunk {
                    chunk,
                    vector,
                    id,
                    doc_hash,
                },
            );
        }

        self.chunks = new_chunks;
        Ok(())
    }

    async fn save_meta_for_store_path(&self, store_path: &Path) -> Result<()> {
        let path = meta_path(store_path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let meta = StoreMetaV2 {
            schema_version: STORE_META_SCHEMA_VERSION,
            model_id: self.model_id.clone(),
            embedding_mode: self.embedding_mode.clone(),
            dimension: self.dimension,
            max_chars: self.templates.max_chars,
            doc_templates: self.templates.document.clone(),
            doc_template_hash: self.templates.doc_template_hash(),
        };
        let data = serde_json::to_vec_pretty(&meta)?;
        tokio::fs::write(path, data).await?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveMode {
    Normal,
    Staged,
}

#[derive(Serialize, Deserialize)]
struct StoreMetaV1 {
    dimension: usize,
}

const STORE_META_SCHEMA_VERSION: u32 = 2;

fn default_embedding_mode() -> String {
    "unknown".to_string()
}

#[derive(Serialize, Deserialize)]
struct StoreMetaV2 {
    schema_version: u32,
    model_id: String,
    #[serde(default = "default_embedding_mode")]
    embedding_mode: String,
    dimension: usize,
    max_chars: usize,
    doc_templates: DocumentTemplates,
    doc_template_hash: u64,
}

#[derive(Clone, Debug)]
struct StoreMetaInfo {
    dimension: usize,
    templates: EmbeddingTemplates,
    doc_template_hash: u64,
    embedding_mode: String,
}

fn meta_path(store_path: &Path) -> PathBuf {
    store_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("meta.json")
}

fn corpus_path_for_store_path(store_path: &Path) -> PathBuf {
    if let Some(dir) = find_context_dir_from_path(store_path) {
        return dir.join("corpus.json");
    }
    default_context_dir_rel().join("corpus.json")
}

async fn load_meta_info(store_path: &Path) -> Option<StoreMetaInfo> {
    let meta_path = meta_path(store_path);
    match tokio::fs::read(&meta_path).await {
        Ok(bytes) => {
            if let Ok(v2) = serde_json::from_slice::<StoreMetaV2>(&bytes) {
                if v2.schema_version == STORE_META_SCHEMA_VERSION {
                    let templates = EmbeddingTemplates {
                        max_chars: v2.max_chars,
                        document: v2.doc_templates,
                        ..EmbeddingTemplates::default()
                    };
                    let hash = v2.doc_template_hash;
                    return Some(StoreMetaInfo {
                        dimension: v2.dimension,
                        templates,
                        doc_template_hash: hash,
                        embedding_mode: v2.embedding_mode,
                    });
                }
            }
            if let Ok(v1) = serde_json::from_slice::<StoreMetaV1>(&bytes) {
                let templates = EmbeddingTemplates::default();
                return Some(StoreMetaInfo {
                    dimension: v1.dimension,
                    doc_template_hash: templates.doc_template_hash(),
                    templates,
                    embedding_mode: default_embedding_mode(),
                });
            }
            None
        }
        Err(_) => None,
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 14_695_981_039_346_656_037;
    const PRIME: u64 = 1_099_511_628_211;
    let mut hash = OFFSET;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn embed_cache_max_bytes_from_env() -> Option<u64> {
    let raw = std::env::var("CONTEXT_EMBED_CACHE_MAX_MB").ok()?;
    let mb: u64 = raw.trim().parse().ok()?;
    if mb == 0 {
        return None;
    }
    Some(mb.saturating_mul(1024).saturating_mul(1024))
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_code_chunker::{ChunkMetadata, CodeChunk};
    use tempfile::TempDir;

    fn create_test_chunk(path: &str, content: &str, line: usize) -> CodeChunk {
        CodeChunk::new(
            path.to_string(),
            line,
            line + 10,
            content.to_string(),
            ChunkMetadata::default(),
        )
    }

    #[tokio::test]
    #[ignore = "Requires model download"]
    async fn test_add_and_search() {
        let temp_dir = TempDir::new().unwrap();
        let store_path = temp_dir.path().join("store.json");
        let mut store = VectorStore::new(&store_path).unwrap();

        let chunks = vec![
            create_test_chunk("test.rs", "fn hello() { println!(\"hello\"); }", 1),
            create_test_chunk("test.rs", "fn goodbye() { println!(\"goodbye\"); }", 15),
        ];

        store.add_chunks(chunks).await.unwrap();
        assert_eq!(store.len(), 2);

        let results = store.search("greeting function", 5).await.unwrap();
        assert!(!results.is_empty());
    }

    #[tokio::test]
    async fn add_chunks_uses_embedding_cache_in_stub_mode() {
        std::env::set_var("CONTEXT_EMBEDDING_MODE", "stub");
        std::env::set_var("CONTEXT_EMBEDDING_MODEL", "bge-small");

        let tmp = TempDir::new().unwrap();
        let store_path = tmp
            .path()
            .join(crate::paths::default_context_dir_rel())
            .join("indexes")
            .join("bge-small")
            .join("index.json");
        tokio::fs::create_dir_all(store_path.parent().unwrap())
            .await
            .unwrap();

        let chunk = create_test_chunk("test.rs", "fn hello() {}", 1);

        let mut store = VectorStore::new_for_model(&store_path, "bge-small").unwrap();
        store.add_chunks(vec![chunk.clone()]).await.unwrap();
        assert_eq!(store.embedder.stub_batch_calls(), Some(1));

        // IndexBundle v3 stores vectors without embedded chunks; keep a corpus alongside the index.
        let corpus_path = super::corpus_path_for_store_path(&store_path);
        let mut corpus = ChunkCorpus::new();
        corpus.set_file_chunks("test.rs".to_string(), vec![chunk.clone()]);
        corpus.save(&corpus_path).await.unwrap();

        store.save().await.unwrap();

        let mut store = VectorStore::load_for_model(&store_path, "bge-small")
            .await
            .unwrap();
        store.remove_chunks_for_file("test.rs");
        store.add_chunks(vec![chunk]).await.unwrap();
        assert_eq!(
            store.embedder.stub_batch_calls(),
            Some(0),
            "expected cache hit to avoid embedding call"
        );
    }
}
