use anyhow::{Context, Result};
use context_code_chunker::CodeChunk;
use context_graph::GraphEdge;
use context_graph::GraphNode;
use context_graph::{CodeGraph, ContextAssembler, GraphLanguage, RelationshipType, Symbol};
use context_vector_store::context_dir_for_project_root;
use log::{debug, warn};
use petgraph::visit::EdgeRef;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;

#[derive(Clone)]
pub struct GraphCache {
    path: PathBuf,
}

impl GraphCache {
    pub fn new(project_root: &Path) -> Self {
        let path = context_dir_for_project_root(project_root).join("graph_cache.json");
        Self { path }
    }

    pub async fn size_bytes(&self) -> Option<u64> {
        tokio::fs::metadata(&self.path).await.ok().map(|m| m.len())
    }

    pub async fn load(
        &self,
        store_mtime: SystemTime,
        language: GraphLanguage,
        chunks: &[CodeChunk],
        chunk_index: &HashMap<String, usize>,
    ) -> Result<Option<ContextAssembler>> {
        if !self.path.exists() {
            return Ok(None);
        }

        let data = match fs::read(&self.path).await {
            Ok(bytes) => bytes,
            Err(err) => {
                warn!("Failed to read graph cache {}: {err}", self.path.display());
                return Ok(None);
            }
        };

        let cached: CachedGraph = match serde_json::from_slice(&data) {
            Ok(cached) => cached,
            Err(err) => {
                warn!("Graph cache corrupted ({}): {err}", self.path.display());
                return Ok(None);
            }
        };

        if cached.language != language {
            debug!(
                "Graph cache language mismatch (cache={:?}, requested={:?})",
                cached.language, language
            );
            return Ok(None);
        }

        if cached.index_mtime_ms != to_unix_ms(store_mtime) {
            debug!("Graph cache stale (mtime mismatch)");
            return Ok(None);
        }

        let mut graph = CodeGraph::new();
        let mut node_indices = Vec::new();

        for node in cached.nodes {
            let Some(&idx) = chunk_index.get(&node.chunk_id) else {
                debug!(
                    "Graph cache chunk {} missing in vector store, forcing rebuild",
                    node.chunk_id
                );
                return Ok(None);
            };
            let Some(chunk) = chunks.get(idx) else {
                debug!(
                    "Graph cache chunk {} index out of bounds, forcing rebuild",
                    node.chunk_id
                );
                return Ok(None);
            };

            let graph_node = GraphNode {
                symbol: node.symbol,
                chunk_id: node.chunk_id,
                chunk: Some(chunk.clone()),
            };
            let idx = graph.add_node(graph_node);
            node_indices.push(idx);
        }

        for edge in cached.edges {
            let Some(&from_idx) = node_indices.get(edge.from) else {
                return Ok(None);
            };
            let Some(&to_idx) = node_indices.get(edge.to) else {
                return Ok(None);
            };
            graph.add_edge(
                from_idx,
                to_idx,
                GraphEdge {
                    relationship: edge.relationship,
                    weight: edge.weight,
                },
            );
        }

        Ok(Some(ContextAssembler::new(graph)))
    }

    pub async fn save(
        &self,
        store_mtime: SystemTime,
        language: GraphLanguage,
        assembler: &ContextAssembler,
    ) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let cached = CachedGraph::from_assembler(store_mtime, language, assembler);
        let data = serde_json::to_vec_pretty(&cached)?;
        fs::write(&self.path, data)
            .await
            .with_context(|| format!("Failed to write graph cache {}", self.path.display()))
    }
}

#[derive(Serialize, Deserialize)]
struct CachedGraph {
    index_mtime_ms: u64,
    language: GraphLanguage,
    nodes: Vec<CachedNode>,
    edges: Vec<CachedEdge>,
}

#[derive(Serialize, Deserialize)]
struct CachedNode {
    symbol: Symbol,
    chunk_id: String,
}

#[derive(Serialize, Deserialize)]
struct CachedEdge {
    from: usize,
    to: usize,
    relationship: RelationshipType,
    weight: f32,
}

impl CachedGraph {
    fn from_assembler(
        store_mtime: SystemTime,
        language: GraphLanguage,
        assembler: &ContextAssembler,
    ) -> Self {
        let graph = assembler.graph();
        let mut node_map = HashMap::new();
        let mut nodes = Vec::new();

        for (idx, node) in graph.graph.node_indices().enumerate() {
            if let Some(data) = graph.graph.node_weight(node) {
                node_map.insert(node, idx);
                nodes.push(CachedNode {
                    symbol: data.symbol.clone(),
                    chunk_id: data.chunk_id.clone(),
                });
            }
        }

        let mut edges = Vec::new();
        for edge in graph.graph.edge_references() {
            if let (Some(&from), Some(&to)) =
                (node_map.get(&edge.source()), node_map.get(&edge.target()))
            {
                edges.push(CachedEdge {
                    from,
                    to,
                    relationship: edge.weight().relationship,
                    weight: edge.weight().weight,
                });
            }
        }

        Self {
            index_mtime_ms: to_unix_ms(store_mtime),
            language,
            nodes,
            edges,
        }
    }
}

fn to_unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
