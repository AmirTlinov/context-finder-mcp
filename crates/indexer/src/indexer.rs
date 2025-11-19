use crate::error::{IndexerError, Result};
use crate::scanner::FileScanner;
use crate::stats::IndexStats;
use context_code_chunker::{Chunker, ChunkerConfig};
use context_vector_store::VectorStore;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

/// Project indexer that scans, chunks, and indexes code
pub struct ProjectIndexer {
    root: PathBuf,
    store_path: PathBuf,
    chunker: Chunker,
}

impl ProjectIndexer {
    /// Create new indexer for project
    pub async fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();

        if !root.exists() {
            return Err(IndexerError::InvalidPath(format!(
                "Path does not exist: {:?}",
                root
            )));
        }

        let store_path = root.join(".context-finder").join("index.json");

        // Create .context-finder directory if needed
        if let Some(parent) = store_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let chunker = Chunker::new(ChunkerConfig::for_embeddings());

        Ok(Self {
            root,
            store_path,
            chunker,
        })
    }

    /// Index the project (with incremental support)
    pub async fn index(&self) -> Result<IndexStats> {
        self.index_with_mode(false).await
    }

    /// Index the project in full mode (skip incremental check)
    pub async fn index_full(&self) -> Result<IndexStats> {
        self.index_with_mode(true).await
    }

    /// Index with specified mode
    async fn index_with_mode(&self, force_full: bool) -> Result<IndexStats> {
        let start = Instant::now();
        let mut stats = IndexStats::new();

        log::info!("Indexing project at {:?}", self.root);

        // 1. Scan for files
        let scanner = FileScanner::new(&self.root);
        let files = scanner.scan()?;

        // 2. Load or create vector store
        let (mut store, existing_mtimes) = if !force_full && self.store_path.exists() {
            log::info!("Loading existing index for incremental update");
            match VectorStore::load(&self.store_path).await {
                Ok(store) => {
                    // Load mtimes from metadata file if exists
                    let mtimes = self.load_mtimes().await.unwrap_or_default();
                    (store, Some(mtimes))
                }
                Err(e) => {
                    log::warn!("Failed to load existing index: {}, starting fresh", e);
                    (VectorStore::new(&self.store_path).await?, None)
                }
            }
        } else {
            (VectorStore::new(&self.store_path).await?, None)
        };

        // 3. Determine which files to process
        let files_to_process = if let Some(ref mtimes_map) = existing_mtimes {
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
        }

        // 4. Process files (parallel for better performance)
        let mut current_mtimes = HashMap::new();

        // Collect mtimes for all files first
        for file_path in &files {
            if let Ok(metadata) = tokio::fs::metadata(&file_path).await {
                if let Ok(modified) = metadata.modified() {
                    if let Ok(duration) = modified.duration_since(SystemTime::UNIX_EPOCH) {
                        current_mtimes.insert(
                            file_path.strip_prefix(&self.root).unwrap_or(&file_path).to_string_lossy().to_string(),
                            duration.as_secs()
                        );
                    }
                }
            }
        }

        // Process changed files in parallel (with concurrency limit)
        if !files_to_process.is_empty() {
            let results = self.process_files_parallel(&files_to_process).await?;

            // Aggregate results
            for result in results {
                match result {
                    Ok((chunks, language, lines)) => {
                        stats.add_file(&language, lines);
                        stats.add_chunks(chunks.len());
                        store.add_chunks(chunks).await?;
                    }
                    Err(e) => {
                        log::warn!("Failed to process file: {}", e);
                        stats.add_error(e);
                    }
                }
            }
        }

        // 5. Save store and mtimes
        store.save().await?;
        self.save_mtimes(&current_mtimes).await?;

        stats.time_ms = start.elapsed().as_millis() as u64;
        log::info!("Indexing completed: {:?}", stats);

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
            let mtime = modified.duration_since(SystemTime::UNIX_EPOCH)?.as_secs();

            let is_changed = existing_mtimes
                .get(&relative_path)
                .map(|&old_mtime| mtime > old_mtime)
                .unwrap_or(true); // New file

            if is_changed {
                changed.push(file_path.clone());
            }
        }

        Ok(changed)
    }

    /// Save file mtimes for incremental indexing
    async fn save_mtimes(&self, mtimes: &HashMap<String, u64>) -> Result<()> {
        let mtimes_path = self.store_path.parent().unwrap().join("mtimes.json");
        let json = serde_json::to_string_pretty(mtimes)?;
        tokio::fs::write(&mtimes_path, json).await?;
        Ok(())
    }

    /// Load file mtimes from previous index
    async fn load_mtimes(&self) -> Result<HashMap<String, u64>> {
        let mtimes_path = self.store_path.parent().unwrap().join("mtimes.json");
        if !mtimes_path.exists() {
            return Ok(HashMap::new());
        }

        let json = tokio::fs::read_to_string(&mtimes_path).await?;
        let mtimes: HashMap<String, u64> = serde_json::from_str(&json)?;
        Ok(mtimes)
    }

    /// Process files in parallel with concurrency limit
    async fn process_files_parallel(
        &self,
        files: &[PathBuf],
    ) -> Result<Vec<std::result::Result<(Vec<context_code_chunker::CodeChunk>, String, usize), String>>> {
        // Parallel file reading (IO bound)
        const MAX_CONCURRENT: usize = 16;

        let mut tasks = Vec::new();

        for file_chunk in files.chunks(MAX_CONCURRENT) {
            for file_path in file_chunk {
                let file_path = file_path.clone();
                let task = tokio::spawn(async move {
                    Self::read_file_static(file_path).await
                });
                tasks.push(task);
            }

            // Wait for this batch and process with chunker
            let mut batch_results = Vec::new();
            for task in tasks.drain(..) {
                match task.await {
                    Ok(Ok((file_path, content, lines))) => {
                        // Process with chunker (CPU bound, sequential per batch)
                        match self.chunker.chunk_str(&content, file_path.to_str()) {
                            Ok(chunks) => {
                                if chunks.is_empty() {
                                    batch_results.push(Ok((vec![], "unknown".to_string(), lines)));
                                } else {
                                    let language = chunks[0]
                                        .metadata
                                        .language
                                        .as_deref()
                                        .unwrap_or("unknown")
                                        .to_string();
                                    batch_results.push(Ok((chunks, language, lines)));
                                }
                            }
                            Err(e) => {
                                batch_results.push(Err(format!("{:?}: {}", file_path, e)));
                            }
                        }
                    }
                    Ok(Err(e)) => batch_results.push(Err(e)),
                    Err(e) => batch_results.push(Err(format!("Task panicked: {}", e))),
                }
            }

            return Ok(batch_results);
        }

        Ok(vec![])
    }

    /// Static method for file reading (IO bound)
    async fn read_file_static(
        file_path: PathBuf,
    ) -> std::result::Result<(PathBuf, String, usize), String> {
        let content = tokio::fs::read_to_string(&file_path)
            .await
            .map_err(|e| format!("{:?}: {}", file_path, e))?;

        let lines = content.lines().count();

        Ok((file_path, content, lines))
    }

    /// Process single file (legacy method, kept for compatibility)
    async fn process_file(
        &self,
        file_path: &Path,
        store: &mut VectorStore,
        stats: &mut IndexStats,
    ) -> Result<()> {
        log::debug!("Processing file: {:?}", file_path);

        let content = tokio::fs::read_to_string(file_path).await?;
        let lines = content.lines().count();

        // Chunk the file
        let chunks = self.chunker.chunk_str(&content, file_path.to_str())?;

        if chunks.is_empty() {
            return Ok(());
        }

        let language = chunks[0]
            .metadata
            .language
            .as_deref()
            .unwrap_or("unknown");

        stats.add_file(language, lines);
        stats.add_chunks(chunks.len());

        // Add to vector store (batch embedding happens here)
        store.add_chunks(chunks).await?;

        Ok(())
    }

    /// Get store path
    pub fn store_path(&self) -> &Path {
        &self.store_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    #[ignore] // Requires FastEmbed model
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
}
