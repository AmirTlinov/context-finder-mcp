use crate::config::ChunkerConfig;
use crate::error::{ChunkerError, Result};
use crate::language::Language;
use crate::types::{ChunkMetadata, ChunkType, CodeChunk};
use tree_sitter::{Node, Parser};

/// AST-based analyzer for semantic code chunking
pub struct AstAnalyzer {
    #[allow(dead_code)]
    config: ChunkerConfig,
    parser: Parser,
    language: Language,
}

impl AstAnalyzer {
    /// Create new AST analyzer for a language
    pub fn new(config: ChunkerConfig, language: Language) -> Result<Self> {
        if !language.supports_ast() {
            return Err(ChunkerError::unsupported_language(language.as_str()));
        }

        let ts_language = language.tree_sitter_language()?;
        let mut parser = Parser::new();
        parser
            .set_language(&ts_language)
            .map_err(|e| ChunkerError::tree_sitter(format!("Failed to set language: {}", e)))?;

        Ok(Self {
            config,
            parser,
            language,
        })
    }

    /// Parse and chunk code using AST
    pub fn chunk(&mut self, content: &str, file_path: &str) -> Result<Vec<CodeChunk>> {
        let tree = self
            .parser
            .parse(content, None)
            .ok_or_else(|| ChunkerError::parse("Failed to parse source code"))?;

        let root = tree.root_node();
        let mut chunks = Vec::new();

        // Extract top-level declarations
        self.extract_chunks(content, file_path, root, &mut chunks)?;

        // If no chunks were extracted, fallback to simple chunking
        if chunks.is_empty() {
            chunks = self.fallback_chunk(content, file_path);
        }

        Ok(chunks)
    }

    /// Extract chunks from AST nodes
    fn extract_chunks(
        &self,
        content: &str,
        file_path: &str,
        node: Node,
        chunks: &mut Vec<CodeChunk>,
    ) -> Result<()> {
        match self.language {
            Language::Rust => self.extract_rust_chunks(content, file_path, node, chunks),
            Language::Python => self.extract_python_chunks(content, file_path, node, chunks),
            Language::JavaScript | Language::TypeScript => {
                self.extract_js_chunks(content, file_path, node, chunks)
            }
            _ => Ok(()),
        }
    }

    /// Extract chunks from Rust code
    fn extract_rust_chunks(
        &self,
        content: &str,
        file_path: &str,
        node: Node,
        chunks: &mut Vec<CodeChunk>,
    ) -> Result<()> {
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();

        for child in children {
            let kind = child.kind();
            let chunk_type = match kind {
                "function_item" => Some(ChunkType::Function),
                "struct_item" => Some(ChunkType::Struct),
                "enum_item" => Some(ChunkType::Enum),
                "impl_item" => Some(ChunkType::Impl),
                "trait_item" => Some(ChunkType::Interface),
                "mod_item" => Some(ChunkType::Module),
                "const_item" => Some(ChunkType::Const),
                "static_item" => Some(ChunkType::Variable),
                _ => None,
            };

            if let Some(chunk_type) = chunk_type {
                // For impl blocks, extract methods separately
                if kind == "impl_item" {
                    self.extract_impl_methods(content, file_path, child, chunks)?;
                } else {
                    let chunk = self.node_to_chunk(content, file_path, child, chunk_type)?;
                    chunks.push(chunk);
                }
            }
        }

        Ok(())
    }

    /// Extract methods from impl block
    fn extract_impl_methods(
        &self,
        content: &str,
        file_path: &str,
        impl_node: Node,
        chunks: &mut Vec<CodeChunk>,
    ) -> Result<()> {
        // Get impl target name (struct/trait being implemented)
        let impl_target = self.extract_impl_target(content, impl_node);

        // Find declaration_list (contains methods in Rust)
        let mut cursor = impl_node.walk();
        for child in impl_node.children(&mut cursor) {
            if child.kind() == "declaration_list" {
                // Walk through declaration_list to find methods
                let mut decl_cursor = child.walk();
                for method_node in child.children(&mut decl_cursor) {
                    let kind = method_node.kind();

                    // Extract methods and associated functions
                    if kind == "function_item" {
                        let mut chunk = self.node_to_chunk(content, file_path, method_node, ChunkType::Method)?;

                        // Set parent scope to impl target
                        if let Some(ref target) = impl_target {
                            chunk.metadata.parent_scope = Some(target.clone());
                        }

                        chunks.push(chunk);
                    } else if kind == "const_item" || kind == "type_item" {
                        // Associated constants and types
                        let chunk_type = if kind == "const_item" {
                            ChunkType::Const
                        } else {
                            ChunkType::Impl // Type aliases in impl
                        };

                        let mut chunk = self.node_to_chunk(content, file_path, method_node, chunk_type)?;

                        if let Some(ref target) = impl_target {
                            chunk.metadata.parent_scope = Some(target.clone());
                        }

                        chunks.push(chunk);
                    }
                }
            }
        }

        Ok(())
    }

    /// Extract the target of an impl block (struct/trait name)
    fn extract_impl_target(&self, content: &str, impl_node: Node) -> Option<String> {
        let mut cursor = impl_node.walk();
        for child in impl_node.children(&mut cursor) {
            // Look for type_identifier (the struct/enum being implemented for)
            if child.kind() == "type_identifier" {
                let start = child.start_byte();
                let end = child.end_byte();
                return Some(content[start..end].to_string());
            }
        }
        None
    }

    /// Extract chunks from Python code
    fn extract_python_chunks(
        &self,
        content: &str,
        file_path: &str,
        node: Node,
        chunks: &mut Vec<CodeChunk>,
    ) -> Result<()> {
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();

        for child in children {
            let kind = child.kind();
            let chunk_type = match kind {
                "function_definition" => Some(ChunkType::Function),
                "class_definition" => Some(ChunkType::Class),
                _ => None,
            };

            if let Some(chunk_type) = chunk_type {
                // For classes, extract methods separately
                if kind == "class_definition" {
                    self.extract_python_class_methods(content, file_path, child, chunks)?;
                } else {
                    let chunk = self.node_to_chunk(content, file_path, child, chunk_type)?;
                    chunks.push(chunk);
                }
            }
        }

        Ok(())
    }

    /// Extract methods from Python class
    fn extract_python_class_methods(
        &self,
        content: &str,
        file_path: &str,
        class_node: Node,
        chunks: &mut Vec<CodeChunk>,
    ) -> Result<()> {
        // Get class name
        let class_name = self.extract_symbol_name(content, class_node);

        // Find the class body (block node)
        let mut cursor = class_node.walk();
        for child in class_node.children(&mut cursor) {
            if child.kind() == "block" {
                // Extract methods from class body
                let mut body_cursor = child.walk();
                for method_node in child.children(&mut body_cursor) {
                    if method_node.kind() == "function_definition" {
                        let mut chunk = self.node_to_chunk(
                            content,
                            file_path,
                            method_node,
                            ChunkType::Method,
                        )?;

                        // Set parent scope to class name
                        if let Some(ref name) = class_name {
                            chunk.metadata.parent_scope = Some(name.clone());
                        }

                        chunks.push(chunk);
                    }
                }
            }
        }

        Ok(())
    }

    /// Extract chunks from JavaScript/TypeScript code
    fn extract_js_chunks(
        &self,
        content: &str,
        file_path: &str,
        node: Node,
        chunks: &mut Vec<CodeChunk>,
    ) -> Result<()> {
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();

        for child in children {
            let kind = child.kind();
            let chunk_type = match kind {
                "function_declaration" => Some(ChunkType::Function),
                "class_declaration" => Some(ChunkType::Class),
                "method_definition" => Some(ChunkType::Method),
                "interface_declaration" => Some(ChunkType::Interface),
                "enum_declaration" => Some(ChunkType::Enum),
                _ => None,
            };

            if let Some(chunk_type) = chunk_type {
                // For classes, extract methods separately
                if kind == "class_declaration" {
                    self.extract_js_class_methods(content, file_path, child, chunks)?;
                } else {
                    let chunk = self.node_to_chunk(content, file_path, child, chunk_type)?;
                    chunks.push(chunk);
                }
            }
        }

        Ok(())
    }

    /// Extract methods from JavaScript/TypeScript class
    fn extract_js_class_methods(
        &self,
        content: &str,
        file_path: &str,
        class_node: Node,
        chunks: &mut Vec<CodeChunk>,
    ) -> Result<()> {
        // Get class name
        let class_name = self.extract_symbol_name(content, class_node);

        // Find the class body
        let mut cursor = class_node.walk();
        for child in class_node.children(&mut cursor) {
            if child.kind() == "class_body" {
                // Extract methods from class body
                let mut body_cursor = child.walk();
                for method_node in child.children(&mut body_cursor) {
                    let method_kind = method_node.kind();

                    // Extract various class members
                    if method_kind == "method_definition"
                        || method_kind == "field_definition"
                        || method_kind == "public_field_definition" {

                        let chunk_type = if method_kind == "method_definition" {
                            ChunkType::Method
                        } else {
                            ChunkType::Variable
                        };

                        let mut chunk = self.node_to_chunk(
                            content,
                            file_path,
                            method_node,
                            chunk_type,
                        )?;

                        // Set parent scope to class name
                        if let Some(ref name) = class_name {
                            chunk.metadata.parent_scope = Some(name.clone());
                        }

                        chunks.push(chunk);
                    }
                }
            }
        }

        Ok(())
    }

    /// Convert AST node to code chunk
    fn node_to_chunk(
        &self,
        content: &str,
        file_path: &str,
        node: Node,
        chunk_type: ChunkType,
    ) -> Result<CodeChunk> {
        let start_byte = node.start_byte();
        let end_byte = node.end_byte();
        let chunk_content = &content[start_byte..end_byte];

        let start_line = node.start_position().row + 1;
        let end_line = node.end_position().row + 1;

        let symbol_name = self.extract_symbol_name(content, node);
        let estimated_tokens = ChunkMetadata::estimate_tokens_from_content(chunk_content);

        let metadata = ChunkMetadata {
            language: Some(self.language.as_str().to_string()),
            chunk_type: Some(chunk_type),
            symbol_name,
            estimated_tokens,
            ..Default::default()
        };

        Ok(CodeChunk::new(
            file_path.to_string(),
            start_line,
            end_line,
            chunk_content.to_string(),
            metadata,
        ))
    }

    /// Extract symbol name from AST node
    fn extract_symbol_name(&self, content: &str, node: Node) -> Option<String> {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            // Different languages use different node kinds for names
            let is_name_node = matches!(
                child.kind(),
                "identifier" | "name" | "type_identifier" | "field_identifier"
            );

            if is_name_node {
                let start = child.start_byte();
                let end = child.end_byte();
                return Some(content[start..end].to_string());
            }
        }
        None
    }

    /// Fallback chunking when AST parsing produces no results
    fn fallback_chunk(&self, content: &str, file_path: &str) -> Vec<CodeChunk> {
        vec![CodeChunk::new(
            file_path.to_string(),
            1,
            content.lines().count(),
            content.to_string(),
            ChunkMetadata {
                language: Some(self.language.as_str().to_string()),
                estimated_tokens: ChunkMetadata::estimate_tokens_from_content(content),
                ..Default::default()
            },
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_chunking() {
        let config = ChunkerConfig::default();
        let mut analyzer = AstAnalyzer::new(config, Language::Rust).unwrap();

        let code = r#"
fn main() {
    println!("Hello");
}

struct Point {
    x: i32,
    y: i32,
}
"#;

        let chunks = analyzer.chunk(code, "test.rs").unwrap();
        assert!(chunks.len() >= 2);

        let has_function = chunks.iter().any(|c| {
            c.metadata.chunk_type == Some(ChunkType::Function)
                && c.metadata.symbol_name.as_deref() == Some("main")
        });
        assert!(has_function);

        let has_struct = chunks.iter().any(|c| {
            c.metadata.chunk_type == Some(ChunkType::Struct)
                && c.metadata.symbol_name.as_deref() == Some("Point")
        });
        assert!(has_struct);
    }

    #[test]
    fn test_python_chunking() {
        let config = ChunkerConfig::default();
        let mut analyzer = AstAnalyzer::new(config, Language::Python).unwrap();

        let code = r#"
def hello():
    print("Hello")

class MyClass:
    def method(self):
        pass
"#;

        let chunks = analyzer.chunk(code, "test.py").unwrap();
        assert!(!chunks.is_empty());
    }

    #[test]
    fn test_unsupported_language() {
        let config = ChunkerConfig::default();
        let result = AstAnalyzer::new(config, Language::Go);
        assert!(result.is_err());
    }
}
