//! # Context Graph
//!
//! Code intelligence through graph analysis of relationships and dependencies.
//!
//! ## Features
//!
//! - **Call graph analysis** - understand function/method call chains
//! - **Dependency tracking** - track imports and type usages
//! - **Relationship mapping** - parent-child, caller-callee relationships
//! - **Smart context assembly** - auto-gather related code for AI agents
//!
//! ## Architecture
//!
//! ```text
//! CodeChunk[]
//!     │
//!     ├──> Graph Builder (AST analysis)
//!     │      ├─ Extract function calls
//!     │      ├─ Extract type references
//!     │      ├─ Extract imports
//!     │      └─ Build edges (relationships)
//!     │
//!     ├──> Code Graph (petgraph)
//!     │      ├─ Nodes: Symbols (functions, classes, methods)
//!     │      └─ Edges: Relationships (calls, uses, extends)
//!     │
//!     └──> Context Assembler
//!            ├─ Find related chunks via graph traversal
//!            ├─ Rank by relevance (distance, type)
//!            └─ Return enriched context for AI agents
//! ```

mod types;
mod builder;
mod graph;
mod assembler;
mod error;

pub use types::{CodeGraph, GraphNode, GraphEdge, RelationshipType, Symbol, SymbolType};
pub use builder::{GraphBuilder, GraphLanguage};
pub use assembler::{ContextAssembler, AssemblyStrategy, AssembledContext, RelatedChunk};
pub use error::{Result, GraphError};
