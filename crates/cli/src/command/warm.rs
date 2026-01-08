use anyhow::Result;
use once_cell::sync::OnceCell;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

use crate::graph_cache::GraphCache;
use context_graph::GraphLanguage;
use context_vector_store::{EmbeddingModel, VectorStore};

#[derive(Debug, Clone, Default)]
pub struct WarmMeta {
    pub warmed: bool,
    pub warm_cost_ms: u64,
    pub graph_cache_hit: bool,
}

#[derive(Clone, Default)]
pub struct Warmer {
    inner: Arc<Mutex<Option<WarmMeta>>>,
}

static GLOBAL_WARMER: OnceCell<Warmer> = OnceCell::new();

pub fn global_warmer() -> Warmer {
    GLOBAL_WARMER.get_or_init(Warmer::default).clone()
}

impl Warmer {
    /// Start prewarm if not already done; returns warm meta (may be cached).
    pub async fn prewarm(&self, project_root: &Path) -> WarmMeta {
        {
            let guard = self.inner.lock().await;
            if let Some(meta) = guard.as_ref() {
                return meta.clone();
            }
        }

        let meta = self.run_warm(project_root).await.unwrap_or_default();
        let mut guard = self.inner.lock().await;
        *guard = Some(meta.clone());
        meta
    }

    async fn run_warm(&self, project_root: &Path) -> Result<WarmMeta> {
        let started = Instant::now();

        let index_path = crate::command::context::index_path(project_root);
        let store = VectorStore::load(&index_path).await?;
        let (chunks, chunk_index) = crate::command::services::collect_chunks(&store);
        let index_mtime = tokio::fs::metadata(&index_path)
            .await
            .ok()
            .and_then(|m| m.modified().ok());

        // Preload graph cache for Rust by default
        let graph_cache = GraphCache::new(project_root);
        let mut graph_cache_hit = false;
        if let Some(mtime) = index_mtime {
            if graph_cache
                .load(mtime, GraphLanguage::Rust, &chunks, &chunk_index)
                .await?
                .is_some()
            {
                graph_cache_hit = true;
            }
        }

        // Trigger lazy embed load
        let model = EmbeddingModel::new()?;
        let _ = model.embed("context warmup").await?;

        Ok(WarmMeta {
            warmed: true,
            warm_cost_ms: started.elapsed().as_millis() as u64,
            graph_cache_hit,
        })
    }
}
