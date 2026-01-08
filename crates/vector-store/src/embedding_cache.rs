use crate::error::Result;
use crate::paths::{find_context_dir_from_path, CONTEXT_DIR_NAME};
use std::path::{Path, PathBuf};

const CACHE_MAGIC: &[u8; 4] = b"EC01";

#[derive(Clone, Debug)]
pub struct EmbeddingCache {
    base_dir: PathBuf,
}

impl EmbeddingCache {
    pub fn for_store_path(store_path: &Path) -> Self {
        let context_dir = find_context_dir_from_path(store_path)
            .unwrap_or_else(|| PathBuf::from(CONTEXT_DIR_NAME));
        Self {
            base_dir: context_dir.join("cache").join("embeddings"),
        }
    }

    pub fn vector_path(
        &self,
        embedding_mode: &str,
        model_id: &str,
        template_hash: u64,
        doc_hash: u64,
    ) -> PathBuf {
        let embed_mode_dir = safe_component(embedding_mode);
        let model_dir = safe_component(model_id);
        let template_dir = format!("{template_hash:016x}");
        let key = format!("{doc_hash:016x}");
        let (shard_a, shard_b) = shard_dirs(&key);
        self.base_dir
            .join(embed_mode_dir)
            .join(model_dir)
            .join(template_dir)
            .join(shard_a)
            .join(shard_b)
            .join(format!("{key}.bin"))
    }

    pub async fn get_vector(
        &self,
        embedding_mode: &str,
        model_id: &str,
        template_hash: u64,
        doc_hash: u64,
        dimension: usize,
    ) -> Option<Vec<f32>> {
        let path = self.vector_path(embedding_mode, model_id, template_hash, doc_hash);
        let bytes = tokio::fs::read(&path).await.ok()?;
        decode_vector(&bytes, dimension)
    }

    pub async fn put_vector(
        &self,
        embedding_mode: &str,
        model_id: &str,
        template_hash: u64,
        doc_hash: u64,
        vector: &[f32],
    ) -> Result<()> {
        let path = self.vector_path(embedding_mode, model_id, template_hash, doc_hash);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bytes = encode_vector(vector);
        let tmp = path.with_extension("bin.tmp");
        tokio::fs::write(&tmp, &bytes).await?;
        if tokio::fs::rename(&tmp, &path).await.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        Ok(())
    }

    pub async fn prune_model_dir(&self, embedding_mode: &str, model_id: &str, max_bytes: u64) {
        if max_bytes == 0 {
            return;
        }
        let root = self
            .base_dir
            .join(safe_component(embedding_mode))
            .join(safe_component(model_id));
        let _ = tokio::task::spawn_blocking(move || prune_dir(&root, max_bytes)).await;
    }
}

fn safe_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "_".to_string()
    } else {
        out
    }
}

fn shard_dirs(hex: &str) -> (String, String) {
    let a = hex.get(0..2).unwrap_or("00").to_string();
    let b = hex.get(2..4).unwrap_or("00").to_string();
    (a, b)
}

fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + vector.len() * 4);
    out.extend_from_slice(CACHE_MAGIC);
    #[allow(clippy::cast_possible_truncation)]
    let dim = vector.len() as u32;
    out.extend_from_slice(&dim.to_le_bytes());
    for v in vector {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn decode_vector(bytes: &[u8], expected_dimension: usize) -> Option<Vec<f32>> {
    if bytes.len() < 8 || &bytes[0..4] != CACHE_MAGIC {
        return None;
    }
    let dim = u32::from_le_bytes(bytes[4..8].try_into().ok()?) as usize;
    if dim != expected_dimension {
        return None;
    }
    let expected_len = 8usize.saturating_add(dim.saturating_mul(4));
    if bytes.len() != expected_len {
        return None;
    }
    let mut vector = Vec::with_capacity(dim);
    for i in 0..dim {
        let start = 8 + i * 4;
        let end = start + 4;
        let val = f32::from_le_bytes(bytes[start..end].try_into().ok()?);
        vector.push(val);
    }
    Some(vector)
}

fn prune_dir(root: &Path, max_bytes: u64) {
    let mut files = Vec::new();
    let mut total = 0u64;
    collect_files(root, &mut files, &mut total);
    if total <= max_bytes {
        return;
    }
    files.sort_by(|a, b| a.modified.cmp(&b.modified));
    for file in files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(&file.path).is_ok() {
            total = total.saturating_sub(file.len);
        }
    }
}

#[derive(Clone)]
struct FileEntry {
    path: PathBuf,
    len: u64,
    modified: std::time::SystemTime,
}

fn collect_files(root: &Path, out: &mut Vec<FileEntry>, total: &mut u64) {
    let Ok(read_dir) = std::fs::read_dir(root) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            collect_files(&path, out, total);
            continue;
        }
        let len = meta.len();
        let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        *total = total.saturating_add(len);
        out.push(FileEntry {
            path,
            len,
            modified,
        });
    }
}
