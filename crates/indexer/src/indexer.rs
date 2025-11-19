use crate::error::{IndexerError, Result};
use crate::scanner::FileScanner;
use crate::stats::IndexStats;
use context_code_chunker::{Chunker, ChunkerConfig};
use context_vector_store::VectorStore;
use std::path::{Path, PathBuf};
use std::time::Instant;

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

    /// Index the project
    pub async fn index(&self) -> Result<IndexStats> {
        let start = Instant::now();
        let mut stats = IndexStats::new();

        log::info!("Indexing project at {:?}", self.root);

        // 1. Scan for files
        let scanner = FileScanner::new(&self.root);
        let files = scanner.scan()?;

        // 2. Create vector store
        let mut store = VectorStore::new(&self.store_path).await?;

        // 3. Process files
        for file_path in files {
            match self.process_file(&file_path, &mut store, &mut stats).await {
                Ok(_) => {}
                Err(e) => {
                    let error_msg = format!("{:?}: {}", file_path, e);
                    log::warn!("Failed to process file: {}", error_msg);
                    stats.add_error(error_msg);
                }
            }
        }

        // 4. Save store
        store.save().await?;

        stats.time_ms = start.elapsed().as_millis() as u64;
        log::info!("Indexing completed: {:?}", stats);

        Ok(stats)
    }

    /// Process single file
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
