use crate::error::Result;
use crate::types::{CodeGraph, RelationshipType};
use context_code_chunker::CodeChunk;
use petgraph::graph::NodeIndex;
use std::collections::{HashMap, HashSet};

/// Smart context assembler for AI agents
///
/// Automatically gathers related code chunks based on graph relationships
pub struct ContextAssembler {
    graph: CodeGraph,
}

/// Context assembly strategy
#[derive(Debug, Clone)]
pub enum AssemblyStrategy {
    /// Include direct dependencies only (depth=1)
    Direct,

    /// Include dependencies and their dependencies (depth=2)
    Extended,

    /// Include full call chain (depth=3)
    Deep,

    /// Custom depth
    Custom(usize),
}

/// Assembled context for AI agent
#[derive(Debug, Clone)]
pub struct AssembledContext {
    /// Primary chunk (the one requested)
    pub primary_chunk: CodeChunk,

    /// Related chunks with relationship info
    pub related_chunks: Vec<RelatedChunk>,

    /// Total context size (for token estimation)
    pub total_lines: usize,
}

#[derive(Debug, Clone)]
pub struct RelatedChunk {
    pub chunk: CodeChunk,
    pub relationship: Vec<RelationshipType>,
    pub distance: usize,
    pub relevance_score: f32,
}

impl ContextAssembler {
    pub fn new(graph: CodeGraph) -> Self {
        Self { graph }
    }

    /// Assemble context for a symbol
    pub fn assemble_for_symbol(
        &self,
        symbol_name: &str,
        strategy: AssemblyStrategy,
    ) -> Result<AssembledContext> {
        let max_depth = match strategy {
            AssemblyStrategy::Direct => 1,
            AssemblyStrategy::Extended => 2,
            AssemblyStrategy::Deep => 3,
            AssemblyStrategy::Custom(d) => d,
        };

        // Find primary node
        let node = self
            .graph
            .find_node(symbol_name)
            .ok_or_else(|| crate::error::GraphError::NodeNotFound(symbol_name.to_string()))?;

        // Get primary chunk
        let primary_node = self
            .graph
            .get_node(node)
            .ok_or_else(|| crate::error::GraphError::NodeNotFound(symbol_name.to_string()))?;

        let primary_chunk = primary_node
            .chunk
            .clone()
            .ok_or_else(|| crate::error::GraphError::BuildError("Missing chunk data".to_string()))?;

        // Get related nodes
        let related_nodes = self.graph.get_related_nodes(node, max_depth);

        // Build related chunks with scores
        let mut related_chunks = Vec::new();
        for (rel_node, distance, path) in related_nodes {
            if let Some(node_data) = self.graph.get_node(rel_node) {
                if let Some(chunk) = &node_data.chunk {
                    let relevance = self.calculate_relevance(distance, &path);
                    related_chunks.push(RelatedChunk {
                        chunk: chunk.clone(),
                        relationship: path,
                        distance,
                        relevance_score: relevance,
                    });
                }
            }
        }

        // Sort by relevance
        related_chunks.sort_by(|a, b| {
            b.relevance_score
                .partial_cmp(&a.relevance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Calculate total lines
        let total_lines = primary_chunk.line_count()
            + related_chunks
                .iter()
                .map(|rc| rc.chunk.line_count())
                .sum::<usize>();

        Ok(AssembledContext {
            primary_chunk,
            related_chunks,
            total_lines,
        })
    }

    /// Assemble context for a chunk ID
    pub fn assemble_for_chunk(
        &self,
        chunk_id: &str,
        strategy: AssemblyStrategy,
    ) -> Result<AssembledContext> {
        // Find nodes for this chunk
        let nodes = self.graph.find_nodes_by_chunk(chunk_id);

        if nodes.is_empty() {
            return Err(crate::error::GraphError::NodeNotFound(chunk_id.to_string()));
        }

        // Use first node's symbol name
        let node = self
            .graph
            .get_node(nodes[0])
            .ok_or_else(|| crate::error::GraphError::NodeNotFound(chunk_id.to_string()))?;

        self.assemble_for_symbol(&node.symbol.name, strategy)
    }

    /// Calculate relevance score based on distance and relationship path
    fn calculate_relevance(&self, distance: usize, path: &[RelationshipType]) -> f32 {
        // Base score decreases with distance
        let distance_score = 1.0 / (distance as f32 + 1.0);

        // Relationship type weights
        let relationship_score: f32 = path
            .iter()
            .map(|rel| match rel {
                RelationshipType::Calls => 1.0,       // Direct call = highest relevance
                RelationshipType::Uses => 0.8,        // Type usage = high relevance
                RelationshipType::Contains => 0.7,    // Parent-child = medium-high
                RelationshipType::Imports => 0.5,     // Import = medium relevance
                RelationshipType::Extends => 0.6,     // Inheritance = medium relevance
                RelationshipType::TestedBy => 0.4,    // Test = lower relevance
            })
            .sum::<f32>()
            / path.len().max(1) as f32;

        distance_score * relationship_score
    }

    /// Get statistics about assembled context
    pub fn get_stats(&self) -> ContextStats {
        ContextStats {
            total_nodes: self.graph.node_count(),
            total_edges: self.graph.edge_count(),
        }
    }

    /// Batch assemble contexts for multiple symbols
    pub fn assemble_batch(
        &self,
        symbol_names: &[&str],
        strategy: AssemblyStrategy,
    ) -> Vec<Result<AssembledContext>> {
        symbol_names
            .iter()
            .map(|name| self.assemble_for_symbol(name, strategy.clone()))
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct ContextStats {
    pub total_nodes: usize,
    pub total_edges: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_relevance() {
        let assembler = ContextAssembler::new(CodeGraph::new());

        // Direct call (distance=1)
        let score1 = assembler.calculate_relevance(1, &[RelationshipType::Calls]);
        assert!(score1 > 0.4);

        // Distant relationship (distance=3)
        let score2 = assembler.calculate_relevance(
            3,
            &[
                RelationshipType::Calls,
                RelationshipType::Uses,
                RelationshipType::Calls,
            ],
        );
        assert!(score2 < score1);
    }
}
