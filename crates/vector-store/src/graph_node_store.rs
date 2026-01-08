use crate::embeddings::EmbeddingModel;
use crate::error::{Result, VectorStoreError};
use crate::hnsw_index::HnswIndex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

pub const GRAPH_NODE_STORE_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone)]
pub struct GraphNodeDoc {
    pub node_id: String,
    pub chunk_id: String,
    pub text: String,
    pub doc_hash: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNodeStoreMeta {
    pub source_index_mtime_ms: u64,
    pub graph_language: String,
    pub graph_doc_version: u32,
    pub template_hash: u64,
    pub model_id: String,
    #[serde(default = "default_embedding_mode")]
    pub embedding_mode: String,
    pub dimension: usize,
}

fn default_embedding_mode() -> String {
    "unknown".to_string()
}

impl GraphNodeStoreMeta {
    pub fn for_current_model(
        source_index_mtime_ms: u64,
        graph_language: impl Into<String>,
        graph_doc_version: u32,
        template_hash: u64,
    ) -> Result<Self> {
        let model_id = crate::current_model_id()?;
        let embedding_mode = crate::embeddings::current_embedding_mode_id()?.to_string();
        let embedder = EmbeddingModel::new()?;
        Ok(Self {
            source_index_mtime_ms,
            graph_language: graph_language.into(),
            graph_doc_version,
            template_hash,
            model_id,
            embedding_mode,
            dimension: embedder.dimension(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct GraphNodeHit {
    pub node_id: String,
    pub chunk_id: String,
    pub score: f32,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedGraphNodeStore {
    schema_version: u32,
    meta: GraphNodeStoreMeta,
    nodes: Vec<PersistedGraphNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedGraphNode {
    node_id: String,
    chunk_id: String,
    doc_hash: u64,
    vector: Vec<f32>,
}

pub struct GraphNodeStore {
    meta: GraphNodeStoreMeta,
    nodes: Vec<PersistedGraphNode>,
    index: HnswIndex,
    embedder: EmbeddingModel,
}

impl GraphNodeStore {
    #[must_use]
    pub const fn meta(&self) -> &GraphNodeStoreMeta {
        &self.meta
    }

    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let bytes = tokio::fs::read(&path).await?;
        let persisted: PersistedGraphNodeStore = serde_json::from_slice(&bytes)?;
        if persisted.schema_version != GRAPH_NODE_STORE_SCHEMA_VERSION {
            return Err(VectorStoreError::EmbeddingError(format!(
                "Unsupported graph_node_store schema_version {} (expected {GRAPH_NODE_STORE_SCHEMA_VERSION})",
                persisted.schema_version
            )));
        }

        let current_model_id = crate::current_model_id()?;
        if persisted.meta.model_id != current_model_id {
            return Err(VectorStoreError::EmbeddingError(format!(
                "GraphNodeStore model mismatch (persisted {} vs current {})",
                persisted.meta.model_id, current_model_id
            )));
        }

        let current_mode = crate::embeddings::current_embedding_mode_id()?.to_string();
        if persisted.meta.embedding_mode != current_mode {
            return Err(VectorStoreError::EmbeddingError(format!(
                "GraphNodeStore embedding mode mismatch (persisted {} vs current {})",
                persisted.meta.embedding_mode, current_mode
            )));
        }

        let embedder = EmbeddingModel::new()?;
        let dimension = embedder.dimension();
        if dimension != persisted.meta.dimension {
            return Err(VectorStoreError::InvalidDimension {
                expected: dimension,
                actual: persisted.meta.dimension,
            });
        }

        let mut index = HnswIndex::new(dimension);
        for (i, node) in persisted.nodes.iter().enumerate() {
            index.add(i, &node.vector)?;
        }

        Ok(Self {
            meta: persisted.meta,
            nodes: persisted.nodes,
            index,
            embedder,
        })
    }

    pub async fn build_or_update(
        path: impl AsRef<Path>,
        meta: GraphNodeStoreMeta,
        docs: Vec<GraphNodeDoc>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let existing = load_persisted_if_compatible(&path, &meta).await?;

        let mut existing_by_id: HashMap<String, PersistedGraphNode> = HashMap::new();
        if let Some(store) = existing {
            for node in store.nodes {
                existing_by_id.insert(node.node_id.clone(), node);
            }
        }

        let embedder = EmbeddingModel::new()?;
        let dimension = embedder.dimension();
        if dimension != meta.dimension {
            return Err(VectorStoreError::InvalidDimension {
                expected: dimension,
                actual: meta.dimension,
            });
        }

        let mut docs = docs;
        docs.sort_by(|a, b| a.node_id.cmp(&b.node_id));

        let mut nodes: Vec<PersistedGraphNode> = Vec::with_capacity(docs.len());
        let mut to_embed: Vec<(usize, String)> = Vec::new();

        for (idx, doc) in docs.into_iter().enumerate() {
            if let Some(prev) = existing_by_id.get(&doc.node_id) {
                if prev.doc_hash == doc.doc_hash {
                    nodes.push(PersistedGraphNode {
                        node_id: doc.node_id,
                        chunk_id: doc.chunk_id,
                        doc_hash: doc.doc_hash,
                        vector: prev.vector.clone(),
                    });
                    continue;
                }
            }

            nodes.push(PersistedGraphNode {
                node_id: doc.node_id,
                chunk_id: doc.chunk_id,
                doc_hash: doc.doc_hash,
                vector: Vec::new(),
            });
            to_embed.push((idx, doc.text));
        }

        if !to_embed.is_empty() {
            let texts: Vec<&str> = to_embed.iter().map(|(_, t)| t.as_str()).collect();
            let vectors = embedder.embed_batch(texts).await?;
            for ((node_idx, _text), vector) in to_embed.into_iter().zip(vectors.into_iter()) {
                if let Some(node) = nodes.get_mut(node_idx) {
                    node.vector = vector;
                }
            }
        }

        let mut index = HnswIndex::new(dimension);
        for (i, node) in nodes.iter().enumerate() {
            index.add(i, &node.vector)?;
        }

        let persisted = PersistedGraphNodeStore {
            schema_version: GRAPH_NODE_STORE_SCHEMA_VERSION,
            meta: meta.clone(),
            nodes: nodes.clone(),
        };
        let data = serde_json::to_vec_pretty(&persisted)?;

        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, data).await?;
        tokio::fs::rename(&tmp, &path).await?;

        Ok(Self {
            meta,
            nodes,
            index,
            embedder,
        })
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<GraphNodeHit>> {
        self.search_with_embedding_text(query, limit).await
    }

    pub async fn search_with_embedding_text(
        &self,
        embedding_text: &str,
        limit: usize,
    ) -> Result<Vec<GraphNodeHit>> {
        if embedding_text.trim().is_empty() {
            return Ok(vec![]);
        }

        let query_vec = self.embedder.embed(embedding_text).await?;
        let neighbors = self.index.search(&query_vec, limit)?;
        Ok(neighbors
            .into_iter()
            .filter_map(|(id, score)| {
                self.nodes.get(id).map(|node| GraphNodeHit {
                    node_id: node.node_id.clone(),
                    chunk_id: node.chunk_id.clone(),
                    score,
                })
            })
            .collect())
    }
}

async fn load_persisted_if_compatible(
    path: &Path,
    desired: &GraphNodeStoreMeta,
) -> Result<Option<PersistedGraphNodeStore>> {
    if !path.exists() {
        return Ok(None);
    }

    let Ok(bytes) = tokio::fs::read(path).await else {
        return Ok(None);
    };
    let Ok(persisted) = serde_json::from_slice::<PersistedGraphNodeStore>(&bytes) else {
        return Ok(None);
    };
    if persisted.schema_version != GRAPH_NODE_STORE_SCHEMA_VERSION {
        return Ok(None);
    }

    let meta = &persisted.meta;
    if meta.graph_language != desired.graph_language
        || meta.graph_doc_version != desired.graph_doc_version
        || meta.template_hash != desired.template_hash
        || meta.model_id != desired.model_id
        || meta.embedding_mode != desired.embedding_mode
        || meta.dimension != desired.dimension
    {
        return Ok(None);
    }

    Ok(Some(persisted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn build_and_search_stub() {
        std::env::set_var("CONTEXT_EMBEDDING_MODE", "stub");
        std::env::set_var("CONTEXT_EMBEDDING_MODEL", "bge-small");

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("graph_nodes.json");

        let embedder = EmbeddingModel::new().unwrap();
        let meta = GraphNodeStoreMeta {
            source_index_mtime_ms: 1,
            graph_language: "rust".to_string(),
            graph_doc_version: 1,
            template_hash: 0,
            model_id: "bge-small".to_string(),
            embedding_mode: "stub".to_string(),
            dimension: embedder.dimension(),
        };

        let docs = vec![
            GraphNodeDoc {
                node_id: "a".to_string(),
                chunk_id: "a.rs:1:2".to_string(),
                text: "alpha beta".to_string(),
                doc_hash: 1,
            },
            GraphNodeDoc {
                node_id: "b".to_string(),
                chunk_id: "b.rs:10:11".to_string(),
                text: "gamma delta".to_string(),
                doc_hash: 2,
            },
        ];

        let store = GraphNodeStore::build_or_update(&path, meta, docs)
            .await
            .unwrap();

        let hits = store.search("alpha", 5).await.unwrap();
        assert!(!hits.is_empty());
    }
}
