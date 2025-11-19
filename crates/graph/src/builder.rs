use crate::error::{GraphError, Result};
use crate::types::*;
use context_code_chunker::CodeChunk;
use petgraph::graph::NodeIndex;
use std::collections::HashMap;
use tree_sitter::{Node, Parser};

/// Supported languages for graph analysis
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphLanguage {
    Rust,
    Python,
    JavaScript,
    TypeScript,
}

/// Build code graph from chunks
pub struct GraphBuilder {
    language: GraphLanguage,
    parser: Parser,
}

impl GraphBuilder {
    pub fn new(language: GraphLanguage) -> Result<Self> {
        let mut parser = Parser::new();

        let ts_lang: tree_sitter::Language = match language {
            GraphLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
            GraphLanguage::Python => tree_sitter_python::LANGUAGE.into(),
            GraphLanguage::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            GraphLanguage::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        };

        parser
            .set_language(&ts_lang)
            .map_err(|e| GraphError::BuildError(format!("Failed to set language: {}", e)))?;

        Ok(Self { language, parser })
    }

    /// Build graph from code chunks
    pub fn build(&mut self, chunks: &[CodeChunk]) -> Result<CodeGraph> {
        let mut graph = CodeGraph::new();

        // Phase 1: Create nodes for all symbols
        let mut chunk_to_node: HashMap<String, NodeIndex> = HashMap::new();

        for chunk in chunks {
            let symbol = self.extract_symbol(chunk)?;
            let chunk_id = format!("{}:{}:{}", chunk.file_path, chunk.start_line, chunk.end_line);

            let node = GraphNode {
                symbol,
                chunk_id: chunk_id.clone(),
                chunk: Some(chunk.clone()),
            };

            let idx = graph.add_node(node);
            chunk_to_node.insert(chunk_id, idx);
        }

        // Phase 2: Analyze relationships and add edges
        for chunk in chunks {
            let chunk_id = format!("{}:{}:{}", chunk.file_path, chunk.start_line, chunk.end_line);

            if let Some(&from_idx) = chunk_to_node.get(&chunk_id) {
                // Extract function calls
                let calls = self.extract_function_calls(chunk)?;
                for called_symbol in calls {
                    if let Some(to_idx) = graph.find_node(&called_symbol) {
                        let edge = GraphEdge {
                            relationship: RelationshipType::Calls,
                            weight: 1.0,
                        };
                        graph.add_edge(from_idx, to_idx, edge);
                    }
                }

                // Extract type usages
                let types = self.extract_type_usages(chunk)?;
                for type_name in types {
                    if let Some(to_idx) = graph.find_node(&type_name) {
                        let edge = GraphEdge {
                            relationship: RelationshipType::Uses,
                            weight: 0.5,
                        };
                        graph.add_edge(from_idx, to_idx, edge);
                    }
                }
            }
        }

        log::info!(
            "Built code graph: {} nodes, {} edges",
            graph.node_count(),
            graph.edge_count()
        );

        Ok(graph)
    }

    /// Extract symbol from chunk
    fn extract_symbol(&self, chunk: &CodeChunk) -> Result<Symbol> {
        let symbol_name = chunk
            .metadata
            .symbol_name
            .clone()
            .unwrap_or_else(|| "unknown".to_string());

        let symbol_type = match chunk.metadata.chunk_type {
            Some(ref ct) => match ct {
                context_code_chunker::ChunkType::Function => SymbolType::Function,
                context_code_chunker::ChunkType::Method => SymbolType::Method,
                context_code_chunker::ChunkType::Class => SymbolType::Class,
                context_code_chunker::ChunkType::Struct => SymbolType::Struct,
                context_code_chunker::ChunkType::Variable => SymbolType::Variable,
                _ => SymbolType::Function,
            },
            None => SymbolType::Function,
        };

        Ok(Symbol {
            name: symbol_name,
            qualified_name: chunk.metadata.qualified_name.clone(),
            file_path: chunk.file_path.clone(),
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            symbol_type,
        })
    }

    /// Extract function calls from chunk (simplified)
    fn extract_function_calls(&mut self, chunk: &CodeChunk) -> Result<Vec<String>> {
        let tree = self
            .parser
            .parse(&chunk.content, None)
            .ok_or_else(|| GraphError::BuildError("Failed to parse chunk".to_string()))?;

        let root = tree.root_node();
        let mut calls = Vec::new();

        self.traverse_for_calls(root, &chunk.content, &mut calls);

        Ok(calls)
    }

    /// Traverse AST for function calls
    fn traverse_for_calls(&self, node: Node, content: &str, calls: &mut Vec<String>) {
        let kind = node.kind();

        // Language-specific call patterns
        let is_call = match self.language {
            GraphLanguage::Rust => kind == "call_expression",
            GraphLanguage::Python => kind == "call",
            GraphLanguage::JavaScript | GraphLanguage::TypeScript => kind == "call_expression",
        };

        if is_call {
            // Extract function name from call
            if let Some(function_node) = node.child_by_field_name("function") {
                let name = self.extract_identifier(function_node, content);
                if !name.is_empty() {
                    calls.push(name);
                }
            }
        }

        // Recursively traverse children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.traverse_for_calls(child, content, calls);
        }
    }

    /// Extract identifier name from node
    fn extract_identifier(&self, node: Node, content: &str) -> String {
        match node.kind() {
            "identifier" | "field_expression" => {
                let start = node.start_byte();
                let end = node.end_byte();
                content[start..end].to_string()
            }
            _ => {
                // Try to find identifier child
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "identifier" {
                        let start = child.start_byte();
                        let end = child.end_byte();
                        return content[start..end].to_string();
                    }
                }
                String::new()
            }
        }
    }

    /// Extract type usages from chunk (simplified)
    fn extract_type_usages(&mut self, chunk: &CodeChunk) -> Result<Vec<String>> {
        let tree = self
            .parser
            .parse(&chunk.content, None)
            .ok_or_else(|| GraphError::BuildError("Failed to parse chunk".to_string()))?;

        let root = tree.root_node();
        let mut types = Vec::new();

        self.traverse_for_types(root, &chunk.content, &mut types);

        Ok(types)
    }

    /// Traverse AST for type references
    fn traverse_for_types(&self, node: Node, content: &str, types: &mut Vec<String>) {
        let kind = node.kind();

        // Language-specific type patterns
        let is_type = match self.language {
            GraphLanguage::Rust => kind == "type_identifier" || kind == "generic_type",
            GraphLanguage::Python => kind == "type",
            GraphLanguage::JavaScript | GraphLanguage::TypeScript => kind == "type_identifier",
        };

        if is_type {
            let start = node.start_byte();
            let end = node.end_byte();
            let type_name = content[start..end].to_string();
            if !type_name.is_empty() {
                types.push(type_name);
            }
        }

        // Recursively traverse children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.traverse_for_types(child, content, types);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_code_chunker::{ChunkMetadata, ChunkType};

    fn create_test_chunk(path: &str, content: &str, symbol: &str, line: usize) -> CodeChunk {
        CodeChunk::new(
            path.to_string(),
            line,
            line + 10,
            content.to_string(),
            ChunkMetadata::default()
                .symbol_name(symbol)
                .chunk_type(ChunkType::Function),
        )
    }

    #[test]
    fn test_build_simple_graph() {
        let chunks = vec![
            create_test_chunk("test.rs", "fn foo() { bar(); }", "foo", 1),
            create_test_chunk("test.rs", "fn bar() {}", "bar", 10),
        ];

        let mut builder = GraphBuilder::new(GraphLanguage::Rust).unwrap();
        let graph = builder.build(&chunks).unwrap();

        assert_eq!(graph.node_count(), 2);
        // Should have edge from foo -> bar (call relationship)
    }
}
