use crate::error::{Result, VectorStoreError};
use crate::paths::context_dir_for_project_root;
use context_code_chunker::CodeChunk;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

pub const CHUNK_CORPUS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Default)]
pub struct ChunkCorpus {
    files: BTreeMap<String, Vec<CodeChunk>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedChunkCorpus {
    schema_version: u32,
    files: BTreeMap<String, Vec<CodeChunk>>,
}

impl ChunkCorpus {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = tokio::fs::read(path).await?;
        let persisted: PersistedChunkCorpus = serde_json::from_slice(&bytes)?;
        if persisted.schema_version != CHUNK_CORPUS_SCHEMA_VERSION {
            return Err(VectorStoreError::EmbeddingError(format!(
                "Unsupported chunk corpus schema_version {} (expected {CHUNK_CORPUS_SCHEMA_VERSION})",
                persisted.schema_version
            )));
        }
        Ok(Self {
            files: persisted.files,
        })
    }

    pub async fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let persisted = PersistedChunkCorpus {
            schema_version: CHUNK_CORPUS_SCHEMA_VERSION,
            files: self.files.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&persisted)?;
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    pub fn set_file_chunks(&mut self, file_path: String, chunks: Vec<CodeChunk>) {
        self.files.insert(file_path, chunks);
    }

    pub fn remove_file(&mut self, file_path: &str) -> bool {
        self.files.remove(file_path).is_some()
    }

    pub fn purge_missing_files(&mut self, live_files: &HashSet<String>) -> usize {
        let before = self.files.len();
        self.files.retain(|path, _| live_files.contains(path));
        before.saturating_sub(self.files.len())
    }

    #[must_use]
    pub fn get_chunk(&self, chunk_id: &str) -> Option<&CodeChunk> {
        let (file_path, start_line, end_line) = parse_chunk_id(chunk_id)?;
        let chunks = self.files.get(&file_path)?;
        chunks
            .iter()
            .find(|chunk| chunk.start_line == start_line && chunk.end_line == end_line)
    }

    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    #[must_use]
    pub const fn files(&self) -> &BTreeMap<String, Vec<CodeChunk>> {
        &self.files
    }
}

#[must_use]
pub fn corpus_path_for_project_root(root: &Path) -> PathBuf {
    context_dir_for_project_root(root).join("corpus.json")
}

fn parse_chunk_id(chunk_id: &str) -> Option<(String, usize, usize)> {
    let mut parts = chunk_id.rsplitn(3, ':');
    let end_line = parts.next()?.parse::<usize>().ok()?;
    let start_line = parts.next()?.parse::<usize>().ok()?;
    let file_path = parts.next()?.to_string();
    Some((file_path, start_line, end_line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_code_chunker::ChunkMetadata;
    use tempfile::TempDir;

    fn chunk(file: &str, start: usize, end: usize, text: &str) -> CodeChunk {
        CodeChunk::new(
            file.to_string(),
            start,
            end,
            text.to_string(),
            ChunkMetadata::default(),
        )
    }

    #[tokio::test]
    async fn corpus_roundtrip_and_lookup() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("corpus.json");

        let mut corpus = ChunkCorpus::new();
        corpus.set_file_chunks(
            "a.rs".to_string(),
            vec![chunk("a.rs", 1, 2, "alpha"), chunk("a.rs", 3, 4, "beta")],
        );
        corpus.set_file_chunks("b.rs".to_string(), vec![chunk("b.rs", 10, 12, "gamma")]);
        corpus.save(&path).await.unwrap();

        let loaded = ChunkCorpus::load(&path).await.unwrap();
        assert_eq!(loaded.file_count(), 2);
        assert_eq!(
            loaded.get_chunk("a.rs:1:2").map(|c| c.content.as_str()),
            Some("alpha")
        );
        assert!(loaded.get_chunk("missing.rs:1:2").is_none());
    }
}
