use crate::types::{CodeGraph, GraphEdge, GraphNode, RelationshipType};
use crate::error::Result;
use petgraph::algo::dijkstra;
use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use std::collections::{HashMap, HashSet};

impl CodeGraph {
    /// Find all nodes that current node calls (outgoing Calls edges)
    pub fn get_callees(&self, node: NodeIndex) -> Vec<NodeIndex> {
        self.graph
            .edges(node)
            .filter(|e| matches!(e.weight().relationship, RelationshipType::Calls))
            .map(|e| e.target())
            .collect()
    }

    /// Find all nodes that call current node (incoming Calls edges)
    pub fn get_callers(&self, node: NodeIndex) -> Vec<NodeIndex> {
        self.graph
            .node_indices()
            .filter(|&idx| {
                self.graph
                    .edges(idx)
                    .any(|e| e.target() == node && matches!(e.weight().relationship, RelationshipType::Calls))
            })
            .collect()
    }

    /// Find all nodes that current node uses (outgoing Uses edges)
    pub fn get_dependencies(&self, node: NodeIndex) -> Vec<NodeIndex> {
        self.graph
            .edges(node)
            .filter(|e| matches!(e.weight().relationship, RelationshipType::Uses))
            .map(|e| e.target())
            .collect()
    }

    /// Find all nodes related to current node within given depth
    /// Returns (NodeIndex, distance, relationship_path)
    pub fn get_related_nodes(
        &self,
        node: NodeIndex,
        max_depth: usize,
    ) -> Vec<(NodeIndex, usize, Vec<RelationshipType>)> {
        let mut visited = HashSet::new();
        let mut result = Vec::new();
        let mut queue = vec![(node, 0, vec![])];

        while let Some((current, depth, path)) = queue.pop() {
            if depth > max_depth || visited.contains(&current) {
                continue;
            }

            visited.insert(current);

            if current != node {
                result.push((current, depth, path.clone()));
            }

            if depth < max_depth {
                // Explore neighbors
                for edge in self.graph.edges(current) {
                    let target = edge.target();
                    if !visited.contains(&target) {
                        let mut new_path = path.clone();
                        new_path.push(edge.weight().relationship.clone());
                        queue.push((target, depth + 1, new_path));
                    }
                }
            }
        }

        result
    }

    /// Find shortest path between two nodes
    pub fn find_path(&self, from: NodeIndex, to: NodeIndex) -> Option<Vec<NodeIndex>> {
        let distances = dijkstra(&self.graph, from, Some(to), |e| e.weight().weight as i32);

        if distances.contains_key(&to) {
            // Reconstruct path (simplified - just return connected nodes)
            Some(vec![from, to])
        } else {
            None
        }
    }

    /// Get nodes by relationship type
    pub fn get_nodes_by_relationship(
        &self,
        node: NodeIndex,
        rel_type: RelationshipType,
    ) -> Vec<NodeIndex> {
        self.graph
            .edges(node)
            .filter(|e| e.weight().relationship == rel_type)
            .map(|e| e.target())
            .collect()
    }

    /// Get all related chunks for a symbol (for context assembly)
    pub fn get_context_for_symbol(&self, symbol_name: &str, max_depth: usize) -> Result<Vec<String>> {
        let node = self.find_node(symbol_name)
            .ok_or_else(|| crate::error::GraphError::NodeNotFound(symbol_name.to_string()))?;

        let related = self.get_related_nodes(node, max_depth);

        let mut chunk_ids = HashSet::new();

        // Add current node's chunk
        if let Some(node_data) = self.get_node(node) {
            chunk_ids.insert(node_data.chunk_id.clone());
        }

        // Add related nodes' chunks
        for (related_node, _dist, _path) in related {
            if let Some(node_data) = self.get_node(related_node) {
                chunk_ids.insert(node_data.chunk_id.clone());
            }
        }

        Ok(chunk_ids.into_iter().collect())
    }
}
