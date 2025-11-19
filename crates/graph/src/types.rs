use context_code_chunker::CodeChunk;
use petgraph::graph::{DiGraph, NodeIndex};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Symbol in code (function, class, method, etc.)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Symbol {
    /// Symbol name (e.g., "authenticate", "User::new")
    pub name: String,

    /// Fully qualified name (e.g., "auth::service::AuthService::authenticate")
    pub qualified_name: Option<String>,

    /// File path
    pub file_path: String,

    /// Line range
    pub start_line: usize,
    pub end_line: usize,

    /// Symbol type (function, class, method, etc.)
    pub symbol_type: SymbolType,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolType {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Interface,
    Variable,
    Constant,
    Module,
}

/// Type of relationship between symbols
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelationshipType {
    /// A calls B (function call)
    Calls,

    /// A uses type B (type reference)
    Uses,

    /// A imports B (import statement)
    Imports,

    /// A contains B (parent-child, e.g., class contains method)
    Contains,

    /// A extends/implements B (inheritance)
    Extends,

    /// A is tested by B (test relationship)
    TestedBy,
}

/// Node in code graph
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    /// Symbol information
    pub symbol: Symbol,

    /// Associated code chunk
    pub chunk_id: String,

    /// Chunk reference for quick access
    #[serde(skip)]
    pub chunk: Option<CodeChunk>,
}

/// Edge in code graph
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    /// Type of relationship
    pub relationship: RelationshipType,

    /// Weight (importance/frequency)
    pub weight: f32,
}

/// Code graph with relationships
pub struct CodeGraph {
    /// Directed graph (symbol -> symbol with relationships)
    pub graph: DiGraph<GraphNode, GraphEdge>,

    /// Symbol name -> NodeIndex mapping for fast lookup
    pub symbol_index: HashMap<String, NodeIndex>,

    /// Chunk ID -> NodeIndex mapping
    pub chunk_index: HashMap<String, Vec<NodeIndex>>,
}

impl CodeGraph {
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            symbol_index: HashMap::new(),
            chunk_index: HashMap::new(),
        }
    }

    /// Add node to graph
    pub fn add_node(&mut self, node: GraphNode) -> NodeIndex {
        let chunk_id = node.chunk_id.clone();
        let symbol_name = node.symbol.name.clone();

        let idx = self.graph.add_node(node);

        // Update indices
        self.symbol_index.insert(symbol_name, idx);
        self.chunk_index
            .entry(chunk_id)
            .or_insert_with(Vec::new)
            .push(idx);

        idx
    }

    /// Add edge between nodes
    pub fn add_edge(&mut self, from: NodeIndex, to: NodeIndex, edge: GraphEdge) {
        self.graph.add_edge(from, to, edge);
    }

    /// Find node by symbol name
    pub fn find_node(&self, symbol_name: &str) -> Option<NodeIndex> {
        self.symbol_index.get(symbol_name).copied()
    }

    /// Find nodes by chunk ID
    pub fn find_nodes_by_chunk(&self, chunk_id: &str) -> Vec<NodeIndex> {
        self.chunk_index
            .get(chunk_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Get node data
    pub fn get_node(&self, idx: NodeIndex) -> Option<&GraphNode> {
        self.graph.node_weight(idx)
    }

    /// Get all nodes
    pub fn nodes(&self) -> impl Iterator<Item = (NodeIndex, &GraphNode)> {
        self.graph.node_indices().filter_map(move |idx| {
            self.graph.node_weight(idx).map(|node| (idx, node))
        })
    }

    /// Get node count
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Get edge count
    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }
}

impl Default for CodeGraph {
    fn default() -> Self {
        Self::new()
    }
}
