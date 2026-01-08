use crate::error::{IndexerError, Result};
use crate::scanner::FileScanner;
use crate::stats::IndexStats;
use context_code_chunker::{Chunker, ChunkerConfig};
use context_vector_store::current_model_id;
use context_vector_store::EmbeddingTemplates;
use context_vector_store::VectorStore;
use context_vector_store::{context_dir_for_project_root, corpus_path_for_project_root, ChunkCorpus};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::index_lock::acquire_index_write_lock;
use crate::limits::acquire_indexing_permit;
use crate::watermark_io::{probe_git_changed_paths_between_heads, probe_git_state};
use crate::{compute_project_watermark, read_index_watermark, write_index_watermark};
use crate::{PersistedIndexWatermark, Watermark};

#[derive(Clone, Debug)]
pub struct ModelIndexSpec {
    pub model_id: String,
    pub templates: EmbeddingTemplates,
}

impl ModelIndexSpec {
    pub fn new(model_id: impl Into<String>, templates: EmbeddingTemplates) -> Self {
        Self {
            model_id: model_id.into(),
            templates,
        }
    }
}

/// Project indexer that scans, chunks, and indexes code
pub struct ProjectIndexer {
    root: PathBuf,
    store_path: PathBuf,
    model_id: String,
    chunker: Chunker,
    templates: Option<EmbeddingTemplates>,
}

/// Multi-model project indexer that scans/chunks files once and embeds the resulting chunks into
/// multiple model-specific indices.
pub struct MultiModelProjectIndexer {
    root: PathBuf,
    chunker: Chunker,
}

impl ProjectIndexer {
    /// Create new indexer for project
    pub async fn new(root: impl AsRef<Path>) -> Result<Self> {
        let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
        Self::new_with_model_and_templates(root, model_id, None).await
    }

    pub async fn new_for_model(
        root: impl AsRef<Path>,
        model_id: impl Into<String>,
    ) -> Result<Self> {
        Self::new_with_model_and_templates(root, model_id.into(), None).await
    }

    pub async fn new_with_embedding_templates(
        root: impl AsRef<Path>,
        templates: EmbeddingTemplates,
    ) -> Result<Self> {
        let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
        Self::new_with_model_and_templates(root, model_id, Some(templates)).await
    }

    pub async fn new_for_model_with_embedding_templates(
        root: impl AsRef<Path>,
        model_id: impl Into<String>,
        templates: EmbeddingTemplates,
    ) -> Result<Self> {
        Self::new_with_model_and_templates(root, model_id.into(), Some(templates)).await
    }

    async fn new_with_model_and_templates(
        root: impl AsRef<Path>,
        model_id: String,
        templates: Option<EmbeddingTemplates>,
    ) -> Result<Self> {
        let root = root.as_ref().to_path_buf();

        if !root.exists() {
            return Err(IndexerError::InvalidPath(format!(
                "Path does not exist: {}",
                root.display()
            )));
        }

        let model_dir = model_id_dir_name(&model_id);
        let store_path = context_dir_for_project_root(&root)
            .join("indexes")
            .join(model_dir)
            .join("index.json");

        // Create context directory if needed
        if let Some(parent) = store_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let chunker = Chunker::new(ChunkerConfig::for_embeddings());

        Ok(Self {
            root,
            store_path,
            model_id,
            chunker,
            templates,
        })
    }

    /// Index the project (with incremental support)
    pub async fn index(&self) -> Result<IndexStats> {
        self.index_with_mode(false, None).await
    }

    /// Index the project in full mode (skip incremental check)
    pub async fn index_full(&self) -> Result<IndexStats> {
        self.index_with_mode(true, None).await
    }

    /// Index the project with a best-effort time budget.
    ///
    /// Budget enforcement is cooperative and checked between major phases. When the budget is
    /// exceeded, the index is **not** persisted to disk.
    pub async fn index_with_budget(&self, max_duration: Duration) -> Result<IndexStats> {
        self.index_with_mode(false, Some(Instant::now() + max_duration))
            .await
    }

    /// Full index with a best-effort time budget.
    pub async fn index_full_with_budget(&self, max_duration: Duration) -> Result<IndexStats> {
        self.index_with_mode(true, Some(Instant::now() + max_duration))
            .await
    }

    pub(crate) async fn index_changed_paths(&self, paths: &[PathBuf]) -> Result<IndexStats> {
        self.index_changed_paths_with_deadline(paths, None).await
    }

    /// Index with specified mode
    #[allow(clippy::cognitive_complexity)]
    #[allow(clippy::too_many_lines)]
    async fn index_with_mode(
        &self,
        force_full: bool,
        deadline: Option<Instant>,
    ) -> Result<IndexStats> {
        let start = Instant::now();
        let mut stats = IndexStats::new();

        // Serialize index writes per project root across processes/sessions.
        // Read paths stay lock-free (atomic renames), but writes must not race.
        let _write_lock = acquire_index_write_lock(&self.root).await?;
        // Avoid stampedes in shared daemons: bound concurrent indexing across projects.
        let _permit = acquire_indexing_permit().await;

        log::info!("Indexing project at {}", self.root.display());
        check_budget(deadline)?;

        // 1. Scan for files
        let scanner = FileScanner::new(&self.root);
        let files = scanner.scan();
        check_budget(deadline)?;
        let live_files: HashSet<String> = files.iter().map(|p| self.normalize_path(p)).collect();

        let corpus_path = corpus_path_for_project_root(&self.root);
        let (mut corpus, corpus_full_rebuild) = if force_full {
            (ChunkCorpus::new(), true)
        } else if corpus_path.exists() {
            match ChunkCorpus::load(&corpus_path).await {
                Ok(corpus) => (corpus, false),
                Err(err) => {
                    log::warn!(
                        "Failed to load chunk corpus {}: {err}; will rebuild corpus",
                        corpus_path.display()
                    );
                    (ChunkCorpus::new(), true)
                }
            }
        } else {
            (ChunkCorpus::new(), true)
        };
        let mut corpus_dirty = corpus_full_rebuild;

        // 2. Load or create vector store
        let allow_incremental_store =
            !force_full && !corpus_full_rebuild && self.store_path.exists();
        let (mut store, existing_mtimes) = if allow_incremental_store {
            log::info!("Loading existing index for incremental update");
            let loaded = if let Some(templates) = self.templates.clone() {
                VectorStore::load_with_templates_for_model(
                    &self.store_path,
                    templates,
                    &self.model_id,
                )
                .await
            } else {
                VectorStore::load_for_model(&self.store_path, &self.model_id).await
            };
            match loaded {
                Ok(store) => {
                    // Load mtimes from metadata file if exists
                    let mtimes = self.load_mtimes().await.unwrap_or_default();
                    (store, Some(mtimes))
                }
                Err(e) => {
                    log::warn!("Failed to load existing index: {e}, starting fresh");
                    let store = if let Some(templates) = self.templates.clone() {
                        VectorStore::new_with_templates_for_model(
                            &self.store_path,
                            &self.model_id,
                            templates,
                        )?
                    } else {
                        VectorStore::new_for_model(&self.store_path, &self.model_id)?
                    };
                    (store, None)
                }
            }
        } else {
            if corpus_full_rebuild && self.store_path.exists() {
                log::info!(
                    "Chunk corpus rebuild detected; rebuilding semantic index at {}",
                    self.store_path.display()
                );
            }
            let store = if let Some(templates) = self.templates.clone() {
                VectorStore::new_with_templates_for_model(
                    &self.store_path,
                    &self.model_id,
                    templates,
                )?
            } else {
                VectorStore::new_for_model(&self.store_path, &self.model_id)?
            };
            (store, None)
        };
        check_budget(deadline)?;

        // 3. Determine which files to process
        let files_to_process = if corpus_full_rebuild {
            files.clone()
        } else if let Some(ref mtimes_map) = existing_mtimes {
            self.filter_changed_files(&files, mtimes_map).await?
        } else {
            files.clone()
        };

        if existing_mtimes.is_some() {
            log::info!(
                "Incremental: processing {} of {} files",
                files_to_process.len(),
                files.len()
            );

            // Purge chunks that belong to files no longer present in the project (deleted/renamed).
            let removed = store.purge_missing_files(&live_files);
            if removed > 0 {
                log::info!("Purged {removed} stale chunks from deleted files");
            }

            let removed = corpus.purge_missing_files(&live_files);
            if removed > 0 {
                log::info!("Purged {removed} missing files from chunk corpus");
                corpus_dirty = true;
            }
        }

        // 4. Process files (parallel for better performance)
        let mut current_mtimes = HashMap::new();

        // Collect mtimes for all files first
        for file_path in &files {
            if let Ok(metadata) = tokio::fs::metadata(&file_path).await {
                if let Ok(modified) = metadata.modified() {
                    if let Ok(duration) = modified.duration_since(SystemTime::UNIX_EPOCH) {
                        current_mtimes.insert(
                            file_path
                                .strip_prefix(&self.root)
                                .unwrap_or(file_path)
                                .to_string_lossy()
                                .to_string(),
                            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
                        );
                    }
                }
            }
        }

        // Process changed files in parallel (with concurrency limit)
        let changed_rels: HashSet<String> = files_to_process
            .iter()
            .map(|p| self.normalize_path(p))
            .collect();
        let corpus_targets: Vec<PathBuf> = if corpus_full_rebuild {
            files.clone()
        } else {
            files_to_process.clone()
        };

        if !corpus_targets.is_empty() {
            let results = self
                .process_files_parallel(&corpus_targets, deadline)
                .await?;

            // Aggregate results
            for result in results {
                check_budget(deadline)?;
                match result {
                    Ok((relative_path, chunks, language, lines)) => {
                        stats.add_file(&language, lines);
                        stats.add_chunks(chunks.len());

                        corpus.set_file_chunks(relative_path.clone(), chunks.clone());
                        corpus_dirty = true;

                        if changed_rels.contains(&relative_path) {
                            if existing_mtimes.is_some() {
                                store.remove_chunks_for_file(&relative_path);
                            }
                            store.add_chunks(chunks).await?;
                        }
                    }
                    Err(e) => {
                        log::warn!("Failed to process file: {e}");
                        stats.add_error(e);
                    }
                }
            }
        }

        // Self-heal: if the chunk corpus has more chunks than the vector store, we are in a
        // "drift" state (e.g., previous run saved corpus but crashed before saving index).
        //
        // The vector store loader drops vectors that don't exist in corpus, so drift can only be
        // "missing vectors for existing corpus chunks" here. Repair by embedding + inserting the
        // missing chunks so agents don't have to babysit the index.
        let corpus_chunk_count: usize = corpus.files().values().map(Vec::len).sum();
        if store.len() < corpus_chunk_count {
            let missing = corpus_chunk_count.saturating_sub(store.len());
            log::info!(
                "Detected corpus/index drift (missing {missing} chunks); repairing for model {}",
                self.model_id
            );
            repair_missing_corpus_chunks(&mut store, &corpus).await?;
        }

        // 5. Save store and mtimes
        check_budget(deadline)?;
        if corpus_dirty {
            corpus.save(&corpus_path).await?;
        }
        store.save().await?;
        self.save_mtimes(&current_mtimes).await?;
        let watermark = compute_project_watermark(&self.root).await?;
        write_index_watermark(&self.store_path, watermark).await?;

        #[allow(clippy::cast_possible_truncation)]
        {
            stats.time_ms = start.elapsed().as_millis() as u64;
            if stats.time_ms == 0 {
                stats.time_ms = 1;
            }
        }
        log::info!("Indexing completed: {stats:?}");

        Ok(stats)
    }

    async fn index_changed_paths_with_deadline(
        &self,
        paths: &[PathBuf],
        deadline: Option<Instant>,
    ) -> Result<IndexStats> {
        if paths.is_empty() {
            return self.index_with_mode(false, deadline).await;
        }

        let corpus_path = corpus_path_for_project_root(&self.root);
        if !corpus_path.exists() || !self.store_path.exists() {
            return self.index_with_mode(false, deadline).await;
        }

        let mtimes_path = self
            .store_path
            .parent()
            .ok_or_else(|| IndexerError::InvalidPath("store path has no parent".into()))?
            .join("mtimes.json");
        if !mtimes_path.exists() {
            return self.index_with_mode(false, deadline).await;
        }

        let Some(git_state) = probe_git_state(&self.root).await else {
            return self.index_with_mode(false, deadline).await;
        };

        const MAX_DELTA_PATHS: usize = 512;

        let mut force_full_scan = false;
        let mut head_changed_rel: Vec<PathBuf> = Vec::new();
        let stored_watermark = read_index_watermark(&self.store_path).await.ok().flatten();
        if let Some(PersistedIndexWatermark {
            watermark:
                Watermark::Git {
                    git_head,
                    git_dirty,
                    ..
                },
            ..
        }) = stored_watermark
        {
            if git_head != git_state.git_head {
                // If the previous index was built in a dirty state, we can't safely reconcile
                // across a HEAD move without risking silent staleness; fall back to a scan.
                if git_dirty {
                    force_full_scan = true;
                } else if let Some(paths) = probe_git_changed_paths_between_heads(
                    &self.root,
                    &git_head,
                    &git_state.git_head,
                    MAX_DELTA_PATHS,
                )
                .await
                {
                    head_changed_rel = paths;
                } else {
                    force_full_scan = true;
                }
            }
        }

        let mut merged: HashSet<PathBuf> = paths.iter().cloned().collect();
        for rel in &git_state.dirty_paths {
            merged.insert(self.root.join(rel));
        }
        for rel in &head_changed_rel {
            merged.insert(self.root.join(rel));
        }

        for path in &merged {
            if path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(".gitignore"))
            {
                force_full_scan = true;
                break;
            }
        }

        if merged.len() > MAX_DELTA_PATHS {
            force_full_scan = true;
        }
        if force_full_scan {
            return self.index_with_mode(false, deadline).await;
        }

        let started = Instant::now();
        let mut stats = IndexStats::new();

        // Serialize index writes per project root across processes/sessions.
        let _write_lock = acquire_index_write_lock(&self.root).await?;
        // Avoid stampedes in shared daemons: bound concurrent indexing across projects.
        let _permit = acquire_indexing_permit().await;

        let mut corpus = ChunkCorpus::load(&corpus_path).await.map_err(|err| {
            IndexerError::Other(format!(
                "Failed to load chunk corpus {}: {err}; falling back to full scan",
                corpus_path.display()
            ))
        })?;

        let mut store = if let Some(templates) = self.templates.clone() {
            VectorStore::load_with_templates_for_model(&self.store_path, templates, &self.model_id)
                .await
        } else {
            VectorStore::load_for_model(&self.store_path, &self.model_id).await
        }
        .map_err(|err| {
            IndexerError::Other(format!(
                "Failed to load index {}: {err}; falling back to full scan",
                self.store_path.display()
            ))
        })?;

        let mut mtimes = self.load_mtimes().await?;

        let mut always_process_rel_set: HashSet<String> = HashSet::new();
        for rel in &git_state.dirty_paths {
            let mut s = rel.to_string_lossy().to_string();
            if s.contains('\\') {
                s = s.replace('\\', "/");
            }
            always_process_rel_set.insert(s);
        }
        for rel in &head_changed_rel {
            let mut s = rel.to_string_lossy().to_string();
            if s.contains('\\') {
                s = s.replace('\\', "/");
            }
            always_process_rel_set.insert(s);
        }

        let mut candidates: Vec<PathBuf> = merged.into_iter().collect();
        candidates.sort();

        // Build a filtered list of existing indexable files (same rules as the full scanner).
        let scanner = FileScanner::new(&self.root);
        let indexable =
            scanner.filter_paths_with_options(&candidates, crate::ScanOptions::default());

        let mut indexable_rel_set: HashSet<String> = HashSet::new();
        let mut indexable_mtimes: HashMap<String, u64> = HashMap::new();
        let mut indexable_candidates: Vec<PathBuf> = Vec::new();
        for path in indexable {
            check_budget(deadline)?;

            let rel = self.normalize_path(&path);
            indexable_rel_set.insert(rel.clone());
            let Ok(meta) = tokio::fs::metadata(&path).await else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let Ok(duration) = modified.duration_since(SystemTime::UNIX_EPOCH) else {
                continue;
            };
            let mtime_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);

            let always_process = always_process_rel_set.contains(&rel);
            let is_changed = always_process
                || mtimes
                    .get(&rel)
                    .is_none_or(|old| mtime_ms > normalize_mtime_ms(*old));
            if !is_changed {
                continue;
            }

            indexable_mtimes.insert(rel, mtime_ms);
            indexable_candidates.push(path);
        }

        // Remove missing / no-longer-indexable files that were in the delta set.
        let mut removed_any = false;
        for path in &candidates {
            check_budget(deadline)?;

            let Ok(relative) = path.strip_prefix(&self.root) else {
                continue;
            };
            let rel = self.normalize_path(&self.root.join(relative));

            if indexable_mtimes.contains_key(&rel) {
                continue;
            }
            if indexable_rel_set.contains(&rel) {
                continue;
            }
            if path.exists() && !mtimes.contains_key(&rel) && !corpus.files().contains_key(&rel) {
                continue;
            }

            if store.remove_chunks_for_file(&rel) > 0 {
                removed_any = true;
            }
            if corpus.remove_file(&rel) {
                removed_any = true;
            }
            mtimes.remove(&rel);
        }

        let mut corpus_dirty = removed_any;
        let mut store_dirty = removed_any;

        if !indexable_candidates.is_empty() {
            let results = self
                .process_files_parallel(&indexable_candidates, deadline)
                .await?;

            for result in results {
                check_budget(deadline)?;

                match result {
                    Ok((relative_path, chunks, language, lines)) => {
                        stats.add_file(&language, lines);
                        stats.add_chunks(chunks.len());

                        // Update corpus (shared truth) first.
                        corpus.set_file_chunks(relative_path.clone(), chunks.clone());
                        corpus_dirty = true;

                        // Replace embeddings for this file.
                        store.remove_chunks_for_file(&relative_path);
                        store.add_chunks(chunks).await?;
                        store_dirty = true;

                        if let Some(mtime_ms) = indexable_mtimes.get(&relative_path) {
                            mtimes.insert(relative_path, *mtime_ms);
                        }
                    }
                    Err(err) => {
                        stats.add_error(err);
                    }
                }
            }
        }

        // Self-heal drift: ensure semantic index catches up to corpus if a previous run was
        // interrupted.
        let corpus_chunk_count: usize = corpus.files().values().map(Vec::len).sum();
        if store.len() < corpus_chunk_count {
            let missing = corpus_chunk_count.saturating_sub(store.len());
            log::info!(
                "Detected corpus/index drift (missing {missing} chunks); repairing for model {}",
                self.model_id
            );
            repair_missing_corpus_chunks(&mut store, &corpus).await?;
            store_dirty = true;
        }

        if corpus_dirty {
            corpus.save(&corpus_path).await?;
        }
        if store_dirty {
            store.save().await?;
        }
        self.save_mtimes(&mtimes).await?;

        let watermark = Watermark::Git {
            computed_at_unix_ms: Some(git_state.computed_at_unix_ms),
            git_head: git_state.git_head,
            git_dirty: git_state.git_dirty,
            dirty_hash: git_state.dirty_hash,
        };
        write_index_watermark(&self.store_path, watermark).await?;

        #[allow(clippy::cast_possible_truncation)]
        {
            stats.time_ms = started.elapsed().as_millis() as u64;
            if stats.time_ms == 0 {
                stats.time_ms = 1;
            }
        }

        Ok(stats)
    }

    /// Filter files that have changed since last index
    async fn filter_changed_files(
        &self,
        files: &[PathBuf],
        existing_mtimes: &HashMap<String, u64>,
    ) -> Result<Vec<PathBuf>> {
        let mut changed = Vec::new();

        for file_path in files {
            let relative_path = file_path
                .strip_prefix(&self.root)
                .unwrap_or(file_path)
                .to_string_lossy()
                .to_string();

            // Check if file is new or modified
            let metadata = tokio::fs::metadata(file_path).await?;
            let modified = metadata.modified()?;
            let mtime = u64::try_from(modified.duration_since(SystemTime::UNIX_EPOCH)?.as_millis())
                .unwrap_or(u64::MAX);

            let is_changed = existing_mtimes
                .get(&relative_path)
                .is_none_or(|&old_mtime| mtime > normalize_mtime_ms(old_mtime));

            if is_changed {
                changed.push(file_path.clone());
            }
        }

        Ok(changed)
    }

    /// Save file mtimes for incremental indexing
    async fn save_mtimes(&self, mtimes: &HashMap<String, u64>) -> Result<()> {
        let mtimes_path = self
            .store_path
            .parent()
            .ok_or_else(|| IndexerError::InvalidPath("store path has no parent".into()))?
            .join("mtimes.json");
        let json = serde_json::to_string_pretty(mtimes)?;
        let tmp = mtimes_path.with_extension("json.tmp");
        tokio::fs::write(&tmp, json).await?;
        tokio::fs::rename(&tmp, &mtimes_path).await?;
        Ok(())
    }

    /// Load file mtimes from previous index
    async fn load_mtimes(&self) -> Result<HashMap<String, u64>> {
        let mtimes_path = self
            .store_path
            .parent()
            .ok_or_else(|| IndexerError::InvalidPath("store path has no parent".into()))?
            .join("mtimes.json");
        if !mtimes_path.exists() {
            return Ok(HashMap::new());
        }

        let json = tokio::fs::read_to_string(&mtimes_path).await?;
        let mut mtimes: HashMap<String, u64> = serde_json::from_str(&json)?;
        for value in mtimes.values_mut() {
            *value = normalize_mtime_ms(*value);
        }
        Ok(mtimes)
    }

    /// Process files in parallel with concurrency limit
    async fn process_files_parallel(
        &self,
        files: &[PathBuf],
        deadline: Option<Instant>,
    ) -> Result<
        Vec<
            std::result::Result<
                (String, Vec<context_code_chunker::CodeChunk>, String, usize),
                String,
            >,
        >,
    > {
        // File processing is a mix of IO + CPU (chunking). A hardcoded high fan-out can cause
        // unnecessary CPU/RAM spikes during large (re)index runs. Prefer a small, adaptive cap.
        let max_concurrent = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(2, 8);

        if files.is_empty() {
            return Ok(Vec::new());
        }

        let mut aggregated = Vec::with_capacity(files.len());

        for file_chunk in files.chunks(max_concurrent) {
            check_budget(deadline)?;
            let mut tasks = Vec::with_capacity(file_chunk.len());
            for file_path in file_chunk {
                let file_path = file_path.clone();
                let task = tokio::spawn(async move { Self::read_file_static(file_path).await });
                tasks.push(task);
            }

            for task in tasks {
                check_budget(deadline)?;
                match task.await {
                    Ok(Ok((file_path, content, lines))) => {
                        let relative_path = self.normalize_path(&file_path);
                        match self.chunker.chunk_str(&content, Some(&relative_path)) {
                            Ok(chunks) => {
                                if chunks.is_empty() {
                                    aggregated.push(Ok((
                                        relative_path,
                                        vec![],
                                        "unknown".to_string(),
                                        lines,
                                    )));
                                } else {
                                    let language = chunks[0]
                                        .metadata
                                        .language
                                        .as_deref()
                                        .unwrap_or("unknown")
                                        .to_string();
                                    aggregated.push(Ok((relative_path, chunks, language, lines)));
                                }
                            }
                            Err(e) => {
                                aggregated.push(Err(format!("{}: {e}", file_path.display())));
                            }
                        }
                    }
                    Ok(Err(e)) => aggregated.push(Err(e)),
                    Err(e) => aggregated.push(Err(format!("Task panicked: {e}"))),
                }
            }
        }

        Ok(aggregated)
    }

    /// Static method for file reading (IO bound)
    async fn read_file_static(
        file_path: PathBuf,
    ) -> std::result::Result<(PathBuf, String, usize), String> {
        let content = tokio::fs::read_to_string(&file_path)
            .await
            .map_err(|e| format!("{}: {e}", file_path.display()))?;

        let lines = content.lines().count();

        Ok((file_path, content, lines))
    }

    /// Process single file (legacy method, kept for compatibility)
    #[allow(dead_code)]
    async fn process_file(
        &self,
        file_path: &Path,
        store: &mut VectorStore,
        stats: &mut IndexStats,
    ) -> Result<()> {
        log::debug!("Processing file: {}", file_path.display());

        let content = tokio::fs::read_to_string(file_path).await?;
        let lines = content.lines().count();

        // Chunk the file
        let relative_path = self.normalize_path(file_path);
        let chunks = self.chunker.chunk_str(&content, Some(&relative_path))?;

        if chunks.is_empty() {
            return Ok(());
        }

        let language = chunks[0].metadata.language.as_deref().unwrap_or("unknown");

        stats.add_file(language, lines);
        stats.add_chunks(chunks.len());

        // Add to vector store (batch embedding happens here)
        store.add_chunks(chunks).await?;

        Ok(())
    }

    /// Get store path
    #[must_use]
    pub fn store_path(&self) -> &Path {
        &self.store_path
    }

    /// Get project root
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn normalize_path(&self, path: &Path) -> String {
        let relative = path.strip_prefix(&self.root).unwrap_or(path).to_path_buf();
        let mut normalized = relative.to_string_lossy().to_string();
        if normalized.contains('\\') {
            normalized = normalized.replace('\\', "/");
        }
        normalized
    }
}

fn model_id_dir_name(model_id: &str) -> String {
    model_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

const fn normalize_mtime_ms(value: u64) -> u64 {
    // Backward-compatible upgrade: older `mtimes.json` persisted seconds since UNIX epoch.
    // Milliseconds since epoch are ~1e12 in 2025; seconds are ~1e9.
    if value < 100_000_000_000 {
        value.saturating_mul(1000)
    } else {
        value
    }
}

impl MultiModelProjectIndexer {
    #[allow(clippy::unused_async)]
    pub async fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();

        if !root.exists() {
            return Err(IndexerError::InvalidPath(format!(
                "Path does not exist: {}",
                root.display()
            )));
        }

        Ok(Self {
            root,
            chunker: Chunker::new(ChunkerConfig::for_embeddings()),
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Index a project for multiple models.
    ///
    /// Design goals:
    /// - Scan + chunk once (union of changed files across models),
    /// - Keep incremental correctness per model (per-model mtimes + purge),
    /// - Avoid process-global env mutation (explicit `model_id` wiring).
    #[allow(clippy::cognitive_complexity)]
    #[allow(clippy::too_many_lines)]
    pub async fn index_models(
        &self,
        models: &[ModelIndexSpec],
        force_full: bool,
    ) -> Result<IndexStats> {
        struct ModelPlan {
            model_id: String,
            store_path: PathBuf,
            templates: EmbeddingTemplates,
            incremental: bool,
            changed_files: HashSet<String>,
        }

        struct StagedModelPaths {
            dir: PathBuf,
            store_path: PathBuf,
            mtimes_path: PathBuf,
        }

        let started = Instant::now();
        if models.is_empty() {
            return Err(IndexerError::Other(
                "Multi-model indexing requires at least one model".to_string(),
            ));
        }

        // Serialize index writes per project root across processes/sessions.
        let _write_lock = acquire_index_write_lock(&self.root).await?;
        // Avoid stampedes in shared daemons: bound concurrent indexing across projects.
        let _permit = acquire_indexing_permit().await;

        log::info!(
            "Indexing project at {} (models={})",
            self.root.display(),
            models.len()
        );

        // 1. Scan for files once.
        let scanner = FileScanner::new(&self.root);
        let files = scanner.scan();

        let live_files: HashSet<String> = files.iter().map(|p| self.normalize_path(p)).collect();

        let corpus_path = corpus_path_for_project_root(&self.root);
        let (mut corpus, corpus_full_rebuild) = if force_full {
            (ChunkCorpus::new(), true)
        } else if corpus_path.exists() {
            match ChunkCorpus::load(&corpus_path).await {
                Ok(corpus) => (corpus, false),
                Err(err) => {
                    log::warn!(
                        "Failed to load chunk corpus {}: {err}; will rebuild corpus",
                        corpus_path.display()
                    );
                    (ChunkCorpus::new(), true)
                }
            }
        } else {
            (ChunkCorpus::new(), true)
        };
        let mut corpus_dirty = corpus_full_rebuild;

        // Staging: build all new artifacts under a unique directory, then commit by renaming into
        // place. This prevents "corpus/index drift" if indexing is interrupted or errors mid-run.
        let staging_id = format!(
            "tx-{}-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(u64::MAX),
            std::process::id()
        );
        let staging_root = context_dir_for_project_root(&self.root)
            .join(".staging")
            .join(staging_id);
        let _staging_cleanup = StagingCleanup::new(staging_root.clone());
        let staging_corpus_path = staging_root.join("corpus.json");

        // 2. Compute current mtimes for all files once.
        let mut current_mtimes: HashMap<String, u64> = HashMap::new();
        for file_path in &files {
            if let Ok(metadata) = tokio::fs::metadata(&file_path).await {
                if let Ok(modified) = metadata.modified() {
                    if let Ok(duration) = modified.duration_since(SystemTime::UNIX_EPOCH) {
                        current_mtimes.insert(
                            self.normalize_path(file_path),
                            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
                        );
                    }
                }
            }
        }

        // 3. Load per-model mtimes, compute union of changed files.
        let mut plans: Vec<ModelPlan> = Vec::with_capacity(models.len());
        let mut union_changed: HashSet<String> = HashSet::new();
        let mut abs_by_rel: HashMap<String, PathBuf> = HashMap::new();
        for file_path in &files {
            abs_by_rel.insert(self.normalize_path(file_path), file_path.clone());
        }

        for spec in models {
            let model_id = spec.model_id.trim().to_string();
            if model_id.is_empty() {
                return Err(IndexerError::Other(
                    "model_id must not be empty".to_string(),
                ));
            }

            let model_dir = model_id_dir_name(&model_id);
            let store_path = context_dir_for_project_root(&self.root)
                .join("indexes")
                .join(model_dir)
                .join("index.json");
            if let Some(parent) = store_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let mtimes_path = store_path
                .parent()
                .expect("index.json has a parent dir")
                .join("mtimes.json");

            let incremental = !force_full && !corpus_full_rebuild && store_path.exists();
            let existing_mtimes = if incremental && mtimes_path.exists() {
                let json = tokio::fs::read_to_string(&mtimes_path).await?;
                let mut loaded = serde_json::from_str::<HashMap<String, u64>>(&json)?;
                for value in loaded.values_mut() {
                    *value = normalize_mtime_ms(*value);
                }
                loaded
            } else {
                HashMap::new()
            };

            let mut changed_files = HashSet::new();
            if force_full || corpus_full_rebuild || !store_path.exists() {
                // Fresh index: process everything.
                for rel in current_mtimes.keys() {
                    changed_files.insert(rel.clone());
                }
            } else {
                for (rel, mtime) in &current_mtimes {
                    let is_changed = existing_mtimes
                        .get(rel)
                        .is_none_or(|old| *mtime > normalize_mtime_ms(*old));
                    if is_changed {
                        changed_files.insert(rel.clone());
                    }
                }
            }

            union_changed.extend(changed_files.iter().cloned());
            plans.push(ModelPlan {
                model_id,
                store_path,
                templates: spec.templates.clone(),
                incremental,
                changed_files,
            });
        }

        // 4. Chunk the union set once.
        let mut stats = IndexStats::new();
        let mut union_paths: Vec<PathBuf> = if corpus_full_rebuild {
            files.clone()
        } else {
            union_changed
                .iter()
                .filter_map(|rel| abs_by_rel.get(rel).cloned())
                .collect()
        };
        union_paths.sort();

        let processed = if union_paths.is_empty() {
            Vec::new()
        } else {
            self.process_files_parallel(&union_paths).await?
        };

        let mut processed_by_rel: HashMap<String, Vec<context_code_chunker::CodeChunk>> =
            HashMap::new();
        let mut processed_errs: HashMap<String, String> = HashMap::new();

        for result in processed {
            match result {
                Ok((relative_path, chunks, language, lines)) => {
                    stats.add_file(&language, lines);
                    stats.add_chunks(chunks.len());
                    processed_by_rel.insert(relative_path, chunks);
                }
                Err(err) => {
                    stats.add_error(err.clone());
                    // Best-effort: parse "path: error" prefix if present.
                    let rel = err.split_once(':').map(|(p, _)| p.trim().to_string());
                    if let Some(rel) = rel {
                        processed_errs.insert(rel, err);
                    }
                }
            }
        }

        if !corpus_full_rebuild {
            let removed = corpus.purge_missing_files(&live_files);
            if removed > 0 {
                log::info!("Purged {removed} missing files from chunk corpus");
                corpus_dirty = true;
            }
        }

        for (relative_path, chunks) in &processed_by_rel {
            if processed_errs.contains_key(relative_path) {
                continue;
            }
            corpus.set_file_chunks(relative_path.clone(), chunks.clone());
            corpus_dirty = true;
        }

        let corpus_chunk_count: usize = corpus.files().values().map(Vec::len).sum();

        // 5. Apply the chunk deltas per model (embed + update store), but stage all writes.
        if let Some(parent) = staging_corpus_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut staged: Vec<(ModelPlan, StagedModelPaths)> = Vec::with_capacity(plans.len());

        for plan in plans {
            let model_dir = model_id_dir_name(&plan.model_id);
            let staged_dir = staging_root.join("indexes").join(&model_dir);
            tokio::fs::create_dir_all(&staged_dir).await?;

            let staged_paths = StagedModelPaths {
                dir: staged_dir.clone(),
                store_path: staged_dir.join("index.json"),
                mtimes_path: staged_dir.join("mtimes.json"),
            };

            let mut store = if plan.incremental && plan.store_path.exists() {
                let loaded = VectorStore::load_with_templates_for_model(
                    &plan.store_path,
                    plan.templates.clone(),
                    &plan.model_id,
                )
                .await;
                match loaded {
                    Ok(store) => store,
                    Err(e) => {
                        log::warn!(
                            "Failed to load existing index {}: {e}, starting fresh",
                            plan.store_path.display()
                        );
                        VectorStore::new_with_templates_for_model(
                            &plan.store_path,
                            &plan.model_id,
                            plan.templates.clone(),
                        )?
                    }
                }
            } else {
                VectorStore::new_with_templates_for_model(
                    &plan.store_path,
                    &plan.model_id,
                    plan.templates.clone(),
                )?
            };

            if plan.incremental {
                let removed = store.purge_missing_files(&live_files);
                if removed > 0 {
                    log::info!("Purged {removed} stale chunks for model {}", plan.model_id);
                }
            }

            for rel in &plan.changed_files {
                if processed_errs.contains_key(rel) {
                    continue;
                }
                let Some(chunks) = processed_by_rel.get(rel) else {
                    continue;
                };

                if plan.incremental {
                    store.remove_chunks_for_file(rel);
                }

                store.add_chunks(chunks.clone()).await?;
            }

            // Self-heal drift: if corpus has chunks that this model index is missing, embed and
            // insert them. This is critical for the daemon "bootstrap" path where mtimes say
            // "no changes", but a previous interrupted run left the index behind.
            if store.len() < corpus_chunk_count {
                let missing = corpus_chunk_count.saturating_sub(store.len());
                log::info!(
                    "Detected corpus/index drift (missing {missing} chunks); repairing for model {}",
                    plan.model_id
                );
                repair_missing_corpus_chunks(&mut store, &corpus).await?;
            }

            // Stage index + meta into the transaction directory.
            store.save_to_path(&staged_paths.store_path).await?;

            // Stage mtimes for incremental correctness per-model.
            let json = serde_json::to_string_pretty(&current_mtimes)?;
            let tmp = staged_paths.mtimes_path.with_extension("json.tmp");
            tokio::fs::write(&tmp, json).await?;
            tokio::fs::rename(&tmp, &staged_paths.mtimes_path).await?;

            staged.push((plan, staged_paths));
        }

        // Stage corpus last; this is the shared source of truth for chunk payloads.
        if corpus_dirty {
            corpus.save(&staging_corpus_path).await?;
        }

        // Capture a project watermark at the end and stage it for each model store.
        // This is a lightweight "freshness contract" used by the read path to detect stale indices.
        let watermark = compute_project_watermark(&self.root).await?;
        for (_, staged_paths) in &staged {
            write_index_watermark(&staged_paths.store_path, watermark.clone()).await?;
        }

        // Commit: rename staged artifacts into place only after the full run succeeds.
        if corpus_dirty {
            if let Some(parent) = corpus_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::rename(&staging_corpus_path, &corpus_path).await?;
        }

        for (plan, staged_paths) in staged {
            let final_dir = plan
                .store_path
                .parent()
                .expect("index.json has a parent dir");
            tokio::fs::create_dir_all(final_dir).await?;

            for file_name in ["index.json", "meta.json", "mtimes.json", "watermark.json"] {
                let src = staged_paths.dir.join(file_name);
                if !src.exists() {
                    continue;
                }
                let dst = final_dir.join(file_name);
                tokio::fs::rename(&src, &dst).await?;
            }
        }

        #[allow(clippy::cast_possible_truncation)]
        {
            stats.time_ms = started.elapsed().as_millis() as u64;
            if stats.time_ms == 0 {
                stats.time_ms = 1;
            }
        }

        Ok(stats)
    }

    pub(crate) async fn index_models_changed_paths(
        &self,
        models: &[ModelIndexSpec],
        changed_paths: &[PathBuf],
    ) -> Result<IndexStats> {
        if changed_paths.is_empty() {
            return self.index_models(models, false).await;
        }

        let corpus_path = corpus_path_for_project_root(&self.root);
        if !corpus_path.exists() {
            return self.index_models(models, false).await;
        }

        let Some(git_state) = probe_git_state(&self.root).await else {
            return self.index_models(models, false).await;
        };

        const MAX_DELTA_PATHS: usize = 512;

        // If HEAD moved since the last successful run, reconcile via a bounded git-diff delta
        // when possible; otherwise fall back to a scan-based rebuild.
        let primary_model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
        let primary_store_path = context_dir_for_project_root(&self.root)
            .join("indexes")
            .join(model_id_dir_name(&primary_model_id))
            .join("index.json");
        let stored_watermark = read_index_watermark(&primary_store_path)
            .await
            .ok()
            .flatten();
        let mut head_changed_rel: Vec<PathBuf> = Vec::new();
        if let Some(PersistedIndexWatermark {
            watermark:
                Watermark::Git {
                    git_head,
                    git_dirty,
                    ..
                },
            ..
        }) = stored_watermark
        {
            if git_head != git_state.git_head {
                if git_dirty {
                    return self.index_models(models, false).await;
                }

                let Some(paths) = probe_git_changed_paths_between_heads(
                    &self.root,
                    &git_head,
                    &git_state.git_head,
                    MAX_DELTA_PATHS,
                )
                .await
                else {
                    return self.index_models(models, false).await;
                };
                head_changed_rel = paths;
            }
        }

        let mut merged: HashSet<PathBuf> = changed_paths.iter().cloned().collect();
        for rel in &git_state.dirty_paths {
            merged.insert(self.root.join(rel));
        }
        for rel in &head_changed_rel {
            merged.insert(self.root.join(rel));
        }
        for path in &merged {
            if path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(".gitignore"))
            {
                return self.index_models(models, false).await;
            }
        }
        if merged.len() > MAX_DELTA_PATHS {
            return self.index_models(models, false).await;
        }

        let mut model_specs: Vec<ModelIndexSpec> = Vec::new();
        for spec in models {
            if spec.model_id.trim().is_empty() {
                continue;
            }
            model_specs.push(spec.clone());
        }
        if model_specs.is_empty() {
            return Err(IndexerError::Other(
                "Multi-model indexing requires at least one model".to_string(),
            ));
        }

        // Serialize index writes per project root across processes/sessions.
        let _write_lock = acquire_index_write_lock(&self.root).await?;
        // Avoid stampedes in shared daemons: bound concurrent indexing across projects.
        let _permit = acquire_indexing_permit().await;

        let started = Instant::now();
        let mut stats = IndexStats::new();

        let mut corpus = ChunkCorpus::load(&corpus_path).await?;

        // Baseline mtimes: use the primary model's mtimes as the canonical project view.
        let primary_mtimes_path = primary_store_path
            .parent()
            .expect("index.json has a parent dir")
            .join("mtimes.json");
        if !primary_mtimes_path.exists() {
            return self.index_models(models, false).await;
        }
        let json = tokio::fs::read_to_string(&primary_mtimes_path).await?;
        let mut mtimes: HashMap<String, u64> = serde_json::from_str(&json)?;
        for value in mtimes.values_mut() {
            *value = normalize_mtime_ms(*value);
        }

        let mut always_process_rel_set: HashSet<String> = HashSet::new();
        for rel in &git_state.dirty_paths {
            let mut s = rel.to_string_lossy().to_string();
            if s.contains('\\') {
                s = s.replace('\\', "/");
            }
            always_process_rel_set.insert(s);
        }
        for rel in &head_changed_rel {
            let mut s = rel.to_string_lossy().to_string();
            if s.contains('\\') {
                s = s.replace('\\', "/");
            }
            always_process_rel_set.insert(s);
        }

        let mut candidates: Vec<PathBuf> = merged.into_iter().collect();
        candidates.sort();

        // Determine which files we need to update (indexable + changed), and which should be
        // removed (missing or no longer indexable).
        let scanner = FileScanner::new(&self.root);
        let indexable =
            scanner.filter_paths_with_options(&candidates, crate::ScanOptions::default());

        let mut indexable_rel_set: HashSet<String> = HashSet::new();
        let mut indexable_mtimes: HashMap<String, u64> = HashMap::new();
        let mut indexable_candidates: Vec<PathBuf> = Vec::new();
        for path in indexable {
            let rel = self.normalize_path(&path);
            indexable_rel_set.insert(rel.clone());
            let Ok(meta) = tokio::fs::metadata(&path).await else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let Ok(duration) = modified.duration_since(SystemTime::UNIX_EPOCH) else {
                continue;
            };
            let mtime_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);

            let always_process = always_process_rel_set.contains(&rel);
            let is_changed = always_process
                || mtimes
                    .get(&rel)
                    .is_none_or(|old| mtime_ms > normalize_mtime_ms(*old));
            if !is_changed {
                continue;
            }
            indexable_mtimes.insert(rel, mtime_ms);
            indexable_candidates.push(path);
        }

        let mut removed_rels: Vec<String> = Vec::new();
        for path in &candidates {
            let Ok(relative) = path.strip_prefix(&self.root) else {
                continue;
            };
            let rel = self.normalize_path(&self.root.join(relative));
            if indexable_mtimes.contains_key(&rel) {
                continue;
            }
            if indexable_rel_set.contains(&rel) {
                continue;
            }
            if path.exists() && !mtimes.contains_key(&rel) && !corpus.files().contains_key(&rel) {
                continue;
            }

            removed_rels.push(rel);
        }
        removed_rels.sort();
        removed_rels.dedup();

        let processed = if indexable_candidates.is_empty() {
            Vec::new()
        } else {
            self.process_files_parallel(&indexable_candidates).await?
        };

        let mut processed_by_rel: HashMap<String, Vec<context_code_chunker::CodeChunk>> =
            HashMap::new();
        let mut processed_errs: HashMap<String, String> = HashMap::new();

        for result in processed {
            match result {
                Ok((relative_path, chunks, language, lines)) => {
                    stats.add_file(&language, lines);
                    stats.add_chunks(chunks.len());
                    processed_by_rel.insert(relative_path, chunks);
                }
                Err(err) => {
                    stats.add_error(err.clone());
                    let rel = err.split_once(':').map(|(p, _)| p.trim().to_string());
                    if let Some(rel) = rel {
                        processed_errs.insert(rel, err);
                    }
                }
            }
        }

        let mut corpus_dirty = false;
        for rel in &removed_rels {
            if corpus.remove_file(rel) {
                corpus_dirty = true;
            }
            mtimes.remove(rel);
        }

        for (relative_path, chunks) in &processed_by_rel {
            if processed_errs.contains_key(relative_path) {
                continue;
            }
            corpus.set_file_chunks(relative_path.clone(), chunks.clone());
            corpus_dirty = true;
            if let Some(mtime_ms) = indexable_mtimes.get(relative_path) {
                mtimes.insert(relative_path.clone(), *mtime_ms);
            }
        }

        let corpus_chunk_count: usize = corpus.files().values().map(Vec::len).sum();

        // Staging: build all new artifacts under a unique directory, then commit by renaming into
        // place.
        let staging_id = format!(
            "tx-{}-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(u64::MAX),
            std::process::id()
        );
        let staging_root = context_dir_for_project_root(&self.root)
            .join(".staging")
            .join(staging_id);
        let _staging_cleanup = StagingCleanup::new(staging_root.clone());
        let staging_corpus_path = staging_root.join("corpus.json");

        let mut staged_dirs: Vec<(PathBuf, PathBuf)> = Vec::new();

        for spec in model_specs {
            let model_id = spec.model_id.trim().to_string();
            let model_dir = model_id_dir_name(&model_id);
            let store_path = context_dir_for_project_root(&self.root)
                .join("indexes")
                .join(&model_dir)
                .join("index.json");
            if !store_path.exists() {
                return self.index_models(models, false).await;
            }

            let staged_dir = staging_root.join("indexes").join(&model_dir);
            tokio::fs::create_dir_all(&staged_dir).await?;
            let staged_store_path = staged_dir.join("index.json");
            let staged_mtimes_path = staged_dir.join("mtimes.json");

            let mut store = VectorStore::load_with_templates_for_model(
                &store_path,
                spec.templates.clone(),
                &model_id,
            )
            .await
            .unwrap_or_else(|_| {
                VectorStore::new_with_templates_for_model(&store_path, &model_id, spec.templates)
                    .expect("create store")
            });

            for rel in &removed_rels {
                store.remove_chunks_for_file(rel);
            }

            for rel in processed_by_rel.keys() {
                if processed_errs.contains_key(rel) {
                    continue;
                }
                let Some(chunks) = processed_by_rel.get(rel) else {
                    continue;
                };
                store.remove_chunks_for_file(rel);
                store.add_chunks(chunks.clone()).await?;
            }

            if store.len() < corpus_chunk_count {
                repair_missing_corpus_chunks(&mut store, &corpus).await?;
            }

            store.save_to_path(&staged_store_path).await?;

            let json = serde_json::to_string_pretty(&mtimes)?;
            let tmp = staged_mtimes_path.with_extension("json.tmp");
            tokio::fs::write(&tmp, json).await?;
            tokio::fs::rename(&tmp, &staged_mtimes_path).await?;

            let watermark = Watermark::Git {
                computed_at_unix_ms: Some(git_state.computed_at_unix_ms),
                git_head: git_state.git_head.clone(),
                git_dirty: git_state.git_dirty,
                dirty_hash: git_state.dirty_hash,
            };
            write_index_watermark(&staged_store_path, watermark).await?;

            staged_dirs.push((staged_dir, store_path));
        }

        if corpus_dirty {
            if let Some(parent) = staging_corpus_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            corpus.save(&staging_corpus_path).await?;
        }

        // Commit staged corpus first (shared truth), then per-model stores.
        if corpus_dirty {
            if let Some(parent) = corpus_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::rename(&staging_corpus_path, &corpus_path).await?;
        }

        for (staged_dir, final_store_path) in staged_dirs {
            let final_dir = final_store_path
                .parent()
                .expect("index.json has a parent dir");
            tokio::fs::create_dir_all(final_dir).await?;

            for file_name in ["index.json", "meta.json", "mtimes.json", "watermark.json"] {
                let src = staged_dir.join(file_name);
                if !src.exists() {
                    continue;
                }
                let dst = final_dir.join(file_name);
                tokio::fs::rename(&src, &dst).await?;
            }
        }

        #[allow(clippy::cast_possible_truncation)]
        {
            stats.time_ms = started.elapsed().as_millis() as u64;
            if stats.time_ms == 0 {
                stats.time_ms = 1;
            }
        }

        Ok(stats)
    }

    fn normalize_path(&self, path: &Path) -> String {
        let relative = path.strip_prefix(&self.root).unwrap_or(path).to_path_buf();
        let mut normalized = relative.to_string_lossy().to_string();
        if normalized.contains('\\') {
            normalized = normalized.replace('\\', "/");
        }
        normalized
    }

    async fn process_files_parallel(
        &self,
        files: &[PathBuf],
    ) -> Result<
        Vec<
            std::result::Result<
                (String, Vec<context_code_chunker::CodeChunk>, String, usize),
                String,
            >,
        >,
    > {
        // File processing is a mix of IO + CPU (chunking). A hardcoded high fan-out can cause
        // unnecessary CPU/RAM spikes during large (re)index runs. Prefer a small, adaptive cap.
        let max_concurrent = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(2, 8);

        if files.is_empty() {
            return Ok(Vec::new());
        }

        let mut aggregated = Vec::with_capacity(files.len());

        for file_chunk in files.chunks(max_concurrent) {
            let mut tasks = Vec::with_capacity(file_chunk.len());
            for file_path in file_chunk {
                let file_path = file_path.clone();
                let task =
                    tokio::spawn(async move { ProjectIndexer::read_file_static(file_path).await });
                tasks.push(task);
            }

            for task in tasks {
                match task.await {
                    Ok(Ok((file_path, content, lines))) => {
                        let relative_path = self.normalize_path(&file_path);
                        match self.chunker.chunk_str(&content, Some(&relative_path)) {
                            Ok(chunks) => {
                                if chunks.is_empty() {
                                    aggregated.push(Ok((
                                        relative_path,
                                        vec![],
                                        "unknown".to_string(),
                                        lines,
                                    )));
                                } else {
                                    let language = chunks[0]
                                        .metadata
                                        .language
                                        .as_deref()
                                        .unwrap_or("unknown")
                                        .to_string();
                                    aggregated.push(Ok((relative_path, chunks, language, lines)));
                                }
                            }
                            Err(e) => {
                                aggregated.push(Err(format!("{}: {e}", file_path.display())));
                            }
                        }
                    }
                    Ok(Err(e)) => aggregated.push(Err(e)),
                    Err(e) => aggregated.push(Err(format!("Task panicked: {e}"))),
                }
            }
        }

        Ok(aggregated)
    }
}

async fn repair_missing_corpus_chunks(store: &mut VectorStore, corpus: &ChunkCorpus) -> Result<()> {
    const REPAIR_BATCH_SIZE: usize = 64;

    let mut pending: Vec<context_code_chunker::CodeChunk> = Vec::new();

    for chunks in corpus.files().values() {
        for chunk in chunks {
            let id = format!(
                "{}:{}:{}",
                chunk.file_path, chunk.start_line, chunk.end_line
            );
            if store.get_chunk(&id).is_some() {
                continue;
            }
            pending.push(chunk.clone());
            if pending.len() >= REPAIR_BATCH_SIZE {
                store.add_chunks(std::mem::take(&mut pending)).await?;
            }
        }
    }

    if !pending.is_empty() {
        store.add_chunks(pending).await?;
    }

    Ok(())
}

fn check_budget(deadline: Option<Instant>) -> Result<()> {
    if let Some(deadline) = deadline {
        if Instant::now() >= deadline {
            return Err(IndexerError::BudgetExceeded);
        }
    }
    Ok(())
}

struct StagingCleanup {
    path: Option<PathBuf>,
}

impl StagingCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }
}

impl Drop for StagingCleanup {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };

        // Best-effort cleanup: staging dirs must not accumulate even on early returns. Prefer
        // async cleanup when we have a Tokio runtime; fall back to sync removal otherwise.
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(async move {
                let _ = tokio::fs::remove_dir_all(&path).await;
            });
        } else {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    #[ignore = "Requires ONNX embedding model"]
    async fn test_indexing() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");

        tokio::fs::write(
            &test_file,
            r#"
fn hello() {
    println!("hello");
}

struct Point {
    x: i32,
    y: i32,
}
"#,
        )
        .await
        .unwrap();

        let indexer = ProjectIndexer::new(temp_dir.path()).await.unwrap();
        let stats = indexer.index().await.unwrap();

        assert!(stats.files > 0);
        assert!(stats.chunks > 0);
    }

    #[tokio::test]
    async fn multi_model_indexing_is_atomic_on_model_error() {
        // Ensure we don't leave "corpus/index drift" artifacts behind when one of the requested
        // models is invalid (common failure mode during profile edits).
        std::env::set_var("CONTEXT_FINDER_EMBEDDING_MODE", "stub");

        // Point the model registry at the repo's manifest so built-in model ids resolve.
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest_dir
            .ancestors()
            .nth(2)
            .expect("repo root from crates/indexer");
        std::env::set_var(
            "CONTEXT_FINDER_MODEL_DIR",
            repo_root.join("models").to_string_lossy().to_string(),
        );

        let temp_dir = TempDir::new().expect("tempdir");
        tokio::fs::create_dir_all(temp_dir.path().join("src"))
            .await
            .expect("mkdir src");
        tokio::fs::write(
            temp_dir.path().join("src").join("lib.rs"),
            "pub fn alpha() { println!(\"atomic\"); }\n",
        )
        .await
        .expect("write lib.rs");

        let indexer = MultiModelProjectIndexer::new(temp_dir.path())
            .await
            .expect("indexer");
        let templates = EmbeddingTemplates::default();
        let specs = vec![
            ModelIndexSpec::new("bge-small", templates.clone()),
            ModelIndexSpec::new("definitely-not-a-real-model", templates),
        ];

        let result = indexer.index_models(&specs, false).await;
        assert!(result.is_err(), "expected model resolution to fail");

        // No partial commits: corpus + indices must not exist.
        assert!(
            !temp_dir
                .path()
                .join(".context")
                .join("corpus.json")
                .exists(),
            "corpus.json should not be written when indexing fails"
        );
        assert!(
            !temp_dir
                .path()
                .join(".context")
                .join("indexes")
                .join("bge-small")
                .join("index.json")
                .exists(),
            "index.json should not be written when indexing fails"
        );
        assert!(
            !temp_dir
                .path()
                .join(".context")
                .join("indexes")
                .join("bge-small")
                .join("watermark.json")
                .exists(),
            "watermark.json should not be written when indexing fails"
        );
    }
}
