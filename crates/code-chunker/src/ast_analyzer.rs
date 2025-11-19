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
    /// Cached imports for current file
    file_imports: Vec<String>,
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
            .map_err(|e| ChunkerError::tree_sitter(format!("Failed to set language: {e}")))?;

        Ok(Self {
            config,
            parser,
            language,
            file_imports: Vec::new(),
        })
    }

    /// Parse and chunk code using AST
    pub fn chunk(&mut self, content: &str, file_path: &str) -> Result<Vec<CodeChunk>> {
        let tree = self
            .parser
            .parse(content, None)
            .ok_or_else(|| ChunkerError::parse("Failed to parse source code"))?;

        let root = tree.root_node();

        // Extract imports first for context
        self.file_imports = self.extract_imports(content, root);

        let mut chunks = Vec::new();

        // Extract top-level declarations
        self.extract_chunks(content, file_path, root, &mut chunks);

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
    ) {
        match self.language {
            Language::Rust => self.extract_rust_chunks(content, file_path, node, chunks),
            Language::Python => self.extract_python_chunks(content, file_path, node, chunks),
            Language::JavaScript | Language::TypeScript => {
                self.extract_js_chunks(content, file_path, node, chunks);
            }
            _ => {}
        }
    }

    /// Extract chunks from Rust code
    fn extract_rust_chunks(
        &self,
        content: &str,
        file_path: &str,
        node: Node,
        chunks: &mut Vec<CodeChunk>,
    ) {
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
                    self.extract_impl_methods(content, file_path, child, chunks);
                } else {
                    let chunk = self.node_to_chunk(content, file_path, child, chunk_type);
                    chunks.push(chunk);
                }
            }
        }

    }

    /// Extract methods from impl block
    fn extract_impl_methods(
        &self,
        content: &str,
        file_path: &str,
        impl_node: Node,
        chunks: &mut Vec<CodeChunk>,
    ) {
        // Get impl target name (struct/trait being implemented)
        let impl_target = Self::extract_impl_target(content, impl_node);

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
                        let mut chunk = self.node_to_chunk(content, file_path, method_node, ChunkType::Method);

                        // Set parent scope and build qualified name
                        if let Some(ref target) = impl_target {
                            chunk.metadata.parent_scope = Some(target.clone());

                            // Build qualified name: "EmbeddingModel::embed"
                            if let Some(ref method_name) = chunk.metadata.symbol_name {
                                chunk.metadata.qualified_name = Some(format!("{target}::{method_name}"));
                            }
                        }

                        chunks.push(chunk);
                    } else if kind == "const_item" || kind == "type_item" {
                        // Associated constants and types
                        let chunk_type = if kind == "const_item" {
                            ChunkType::Const
                        } else {
                            ChunkType::Impl // Type aliases in impl
                        };

                        let mut chunk = self.node_to_chunk(content, file_path, method_node, chunk_type);

                        if let Some(ref target) = impl_target {
                            chunk.metadata.parent_scope = Some(target.clone());
                        }

                        chunks.push(chunk);
                    }
                }
            }
        }
    }

    /// Extract the target of an impl block (struct/trait name)
    fn extract_impl_target(content: &str, impl_node: Node) -> Option<String> {
        let mut cursor = impl_node.walk();
        for child in impl_node.children(&mut cursor) {
            let kind = child.kind();

            // Handle different type representations
            match kind {
                // Simple type: impl MyStruct
                "type_identifier" => {
                    let start = child.start_byte();
                    let end = child.end_byte();
                    return Some(content[start..end].to_string());
                }

                // Generic type: impl<T> MyStruct<T>
                "generic_type" => {
                    // Get the base type name (before <...>)
                    let mut type_cursor = child.walk();
                    for type_child in child.children(&mut type_cursor) {
                        if type_child.kind() == "type_identifier" {
                            let start = type_child.start_byte();
                            let end = type_child.end_byte();
                            return Some(content[start..end].to_string());
                        }
                    }
                }

                // Qualified path: impl module::MyStruct
                "scoped_type_identifier" => {
                    // Extract the final identifier after ::
                    let mut type_cursor = child.walk();
                    for type_child in child.children(&mut type_cursor) {
                        if type_child.kind() == "type_identifier" {
                            let start = type_child.start_byte();
                            let end = type_child.end_byte();
                            return Some(content[start..end].to_string());
                        }
                    }
                }

                _ => {}
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
    ) {
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
                    self.extract_python_class_methods(content, file_path, child, chunks);
                } else {
                    let chunk = self.node_to_chunk(content, file_path, child, chunk_type);
                    chunks.push(chunk);
                }
            }
        }

    }

    /// Extract methods from Python class
    fn extract_python_class_methods(
        &self,
        content: &str,
        file_path: &str,
        class_node: Node,
        chunks: &mut Vec<CodeChunk>,
    ) {
        // Get class name
        let class_name = Self::extract_symbol_name(content, class_node);

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
                        );

                        // Set parent scope and build qualified name
                        if let Some(ref name) = class_name {
                            chunk.metadata.parent_scope = Some(name.clone());

                            // Build qualified name: "MyClass.method"
                            if let Some(ref method_name) = chunk.metadata.symbol_name {
                                chunk.metadata.qualified_name = Some(format!("{name}.{method_name}"));
                            }
                        }

                        chunks.push(chunk);
                    }
                }
            }
        }
    }

    /// Extract chunks from JavaScript/TypeScript code
    fn extract_js_chunks(
        &self,
        content: &str,
        file_path: &str,
        node: Node,
        chunks: &mut Vec<CodeChunk>,
    ) {
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
                    self.extract_js_class_methods(content, file_path, child, chunks);
                } else {
                    let chunk = self.node_to_chunk(content, file_path, child, chunk_type);
                    chunks.push(chunk);
                }
            }
        }

    }

    /// Extract methods from JavaScript/TypeScript class
    fn extract_js_class_methods(
        &self,
        content: &str,
        file_path: &str,
        class_node: Node,
        chunks: &mut Vec<CodeChunk>,
    ) {
        // Get class name
        let class_name = Self::extract_symbol_name(content, class_node);

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
                        );

                        // Set parent scope to class name
                        if let Some(ref name) = class_name {
                            chunk.metadata.parent_scope = Some(name.clone());
                        }

                        chunks.push(chunk);
                    }
                }
            }
        }
    }

    /// Convert AST node to code chunk with enhanced context
    fn node_to_chunk(
        &self,
        content: &str,
        file_path: &str,
        node: Node,
        chunk_type: ChunkType,
    ) -> CodeChunk {
        // Extract docstrings/comments before the node
        let doc_comments = self.extract_doc_comments(content, node);

        let start_byte = node.start_byte();
        let end_byte = node.end_byte();
        let code_content = &content[start_byte..end_byte];

        let start_line = node.start_position().row + 1;
        let end_line = node.end_position().row + 1;

        let symbol_name = Self::extract_symbol_name(content, node);

        // Build qualified name (will be updated with parent_scope later if method)
        let qualified_name = symbol_name.clone();

        // Select relevant imports (filter by what's used in this chunk)
        let relevant_imports = self.filter_relevant_imports(code_content);

        // Build enhanced content for embedding: imports + docstrings + code
        let mut enhanced_content = String::new();

        // Add relevant imports for context
        if !relevant_imports.is_empty() {
            enhanced_content.push_str(&relevant_imports.join("\n"));
            enhanced_content.push_str("\n\n");
        }

        // Add docstrings
        if !doc_comments.is_empty() {
            enhanced_content.push_str(&doc_comments);
            enhanced_content.push('\n');
        }

        // Add code
        enhanced_content.push_str(code_content);

        let estimated_tokens = ChunkMetadata::estimate_tokens_from_content(&enhanced_content);

        let metadata = ChunkMetadata {
            language: Some(self.language.as_str().to_string()),
            chunk_type: Some(chunk_type),
            symbol_name,
            context_imports: relevant_imports,
            qualified_name,
            estimated_tokens,
            ..Default::default()
        };

        CodeChunk::new(
            file_path.to_string(),
            start_line,
            end_line,
            enhanced_content,
            metadata,
        )
    }

    /// Filter imports to only those relevant to this chunk
    fn filter_relevant_imports(&self, code_content: &str) -> Vec<String> {
        let mut relevant = Vec::new();

        for import in &self.file_imports {
            // Extract identifiers from import
            let identifiers = self.extract_identifiers_from_import(import);

            // Check if any identifier is used in code
            for ident in identifiers {
                if code_content.contains(&ident) {
                    relevant.push(import.clone());
                    break;
                }
            }

            // Limit to avoid bloat
            if relevant.len() >= 5 {
                break;
            }
        }

        relevant
    }

    /// Extract identifiers from import statement
    fn extract_identifiers_from_import(&self, import: &str) -> Vec<String> {
        let mut identifiers = Vec::new();

        match self.language {
            Language::Rust => {
                // use std::collections::HashMap -> HashMap
                // use crate::error::{Result, Error} -> Result, Error
                if let Some(last_part) = import.split("::").last() {
                    // Handle {A, B, C}
                    if last_part.contains('{') {
                        let inner = last_part
                            .trim_start_matches('{')
                            .trim_end_matches('}');
                        for ident in inner.split(',') {
                            identifiers.push(ident.trim().to_string());
                        }
                    } else {
                        identifiers.push(last_part.trim().to_string());
                    }
                }
            }
            Language::Python => {
                // from x import A, B -> A, B
                // import x -> x
                if import.contains("import") {
                    if let Some(after_import) = import.split("import").nth(1) {
                        for ident in after_import.split(',') {
                            identifiers.push(ident.trim().to_string());
                        }
                    }
                }
            }
            Language::JavaScript | Language::TypeScript => {
                // import { A, B } from 'x' -> A, B
                if import.contains('{') {
                    if let Some(inner_start) = import.find('{') {
                        if let Some(inner_end) = import.find('}') {
                            let inner = &import[inner_start + 1..inner_end];
                            for ident in inner.split(',') {
                                identifiers.push(ident.trim().to_string());
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        identifiers
    }

    /// Extract documentation comments/docstrings before a node
    /// Uses text-based parsing since Tree-sitter doesn't include comments in AST
    fn extract_doc_comments(&self, content: &str, node: Node) -> String {
        let node_start_line = node.start_position().row;
        let lines: Vec<&str> = content.lines().collect();

        if node_start_line == 0 || node_start_line >= lines.len() {
            return String::new();
        }

        let mut doc_lines = Vec::new();

        // Scan backwards from the node's start line to find doc comments
        let mut line_idx = node_start_line;
        while line_idx > 0 {
            line_idx -= 1;
            let line = lines[line_idx].trim();

            // Check for doc comments based on language
            let is_doc = match self.language {
                Language::Rust => {
                    // Rust doc comments: ///, //!, /** */, etc.
                    line.starts_with("///") || line.starts_with("//!") || line.starts_with("/**")
                }
                Language::Python => {
                    // Python doc comments: # or """ docstrings
                    line.starts_with('#') || line.starts_with("\"\"\"") || line.starts_with("'''")
                }
                Language::JavaScript | Language::TypeScript => {
                    // JS/TS doc comments: //, /* */
                    line.starts_with("//") || line.starts_with("/*") || line.starts_with('*')
                }
                _ => false,
            };

            if is_doc {
                doc_lines.push(lines[line_idx]);
            } else if !line.is_empty() {
                // Stop if we hit a non-empty, non-comment line
                break;
            }
        }

        // Reverse to restore original order
        doc_lines.reverse();
        doc_lines.join("\n")
    }


    /// Extract imports/dependencies from file for context
    fn extract_imports(&self, content: &str, root: Node) -> Vec<String> {
        let mut imports = Vec::new();
        let mut cursor = root.walk();

        for child in root.children(&mut cursor) {
            let kind = child.kind();

            // Language-specific import nodes
            let is_import = match self.language {
                Language::Rust => kind == "use_declaration",
                Language::Python => kind == "import_statement" || kind == "import_from_statement",
                Language::JavaScript | Language::TypeScript => {
                    kind == "import_statement" || kind == "import"
                }
                _ => false,
            };

            if is_import {
                let start = child.start_byte();
                let end = child.end_byte();
                let import_text = content[start..end].trim().to_string();

                // Clean up import text (remove trailing semicolons, etc.)
                let cleaned = import_text
                    .trim_end_matches(';')
                    .lines()
                    .next()
                    .unwrap_or(&import_text)
                    .to_string();

                if !cleaned.is_empty() {
                    imports.push(cleaned);
                }
            }
        }

        // Limit imports to avoid bloat
        imports.truncate(20);
        imports
    }

    /// Extract symbol name from AST node
    fn extract_symbol_name(content: &str, node: Node) -> Option<String> {
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
