use crate::error::{ChunkerError, Result};
use std::path::Path;

/// Supported programming language
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    Ruby,
    Swift,
    Kotlin,
    Unknown,
}

impl Language {
    /// Detect language from file extension
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            "rs" => Language::Rust,
            "py" | "pyw" => Language::Python,
            "js" | "mjs" | "cjs" => Language::JavaScript,
            "ts" | "tsx" => Language::TypeScript,
            "go" => Language::Go,
            "java" => Language::Java,
            "c" | "h" => Language::C,
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Language::Cpp,
            "cs" => Language::CSharp,
            "rb" => Language::Ruby,
            "swift" => Language::Swift,
            "kt" | "kts" => Language::Kotlin,
            _ => Language::Unknown,
        }
    }

    /// Detect language from file path
    pub fn from_path(path: impl AsRef<Path>) -> Self {
        path.as_ref()
            .extension()
            .and_then(|ext| ext.to_str())
            .map(Self::from_extension)
            .unwrap_or(Language::Unknown)
    }

    /// Get language name as string
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Go => "go",
            Language::Java => "java",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::CSharp => "csharp",
            Language::Ruby => "ruby",
            Language::Swift => "swift",
            Language::Kotlin => "kotlin",
            Language::Unknown => "unknown",
        }
    }

    /// Check if this language is supported for AST parsing
    pub fn supports_ast(self) -> bool {
        matches!(
            self,
            Language::Rust
                | Language::Python
                | Language::JavaScript
                | Language::TypeScript
        )
    }

    /// Get Tree-sitter language instance
    pub fn tree_sitter_language(self) -> Result<tree_sitter::Language> {
        match self {
            Language::Rust => Ok(tree_sitter_rust::LANGUAGE.into()),
            Language::Python => Ok(tree_sitter_python::LANGUAGE.into()),
            Language::JavaScript => Ok(tree_sitter_javascript::LANGUAGE.into()),
            Language::TypeScript => {
                Ok(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            }
            _ => Err(ChunkerError::unsupported_language(self.as_str())),
        }
    }

    /// Get typical comment prefixes for this language
    pub fn comment_prefixes(self) -> Vec<&'static str> {
        match self {
            Language::Rust
            | Language::JavaScript
            | Language::TypeScript
            | Language::Go
            | Language::Java
            | Language::C
            | Language::Cpp
            | Language::CSharp
            | Language::Swift
            | Language::Kotlin => vec!["//", "/*", "///", "/**"],
            Language::Python | Language::Ruby => vec!["#", "\"\"\"", "'''"],
            Language::Unknown => vec![],
        }
    }

    /// Get import/use statement patterns for this language
    pub fn import_patterns(self) -> Vec<&'static str> {
        match self {
            Language::Rust => vec!["use ", "extern crate "],
            Language::Python => vec!["import ", "from "],
            Language::JavaScript | Language::TypeScript => vec!["import ", "require("],
            Language::Go => vec!["import "],
            Language::Java => vec!["import "],
            Language::CSharp => vec!["using "],
            Language::Ruby => vec!["require ", "include "],
            Language::Swift => vec!["import "],
            Language::Kotlin => vec!["import "],
            Language::C | Language::Cpp => vec!["#include "],
            Language::Unknown => vec![],
        }
    }

    /// Get typical file size thresholds for this language
    pub fn size_limits(self) -> LanguageSizeLimits {
        match self {
            Language::Python | Language::Ruby => LanguageSizeLimits {
                typical_lines: 200,
                large_lines: 500,
                huge_lines: 1000,
            },
            Language::Rust | Language::Go => LanguageSizeLimits {
                typical_lines: 300,
                large_lines: 600,
                huge_lines: 1200,
            },
            Language::JavaScript | Language::TypeScript => LanguageSizeLimits {
                typical_lines: 150,
                large_lines: 400,
                huge_lines: 800,
            },
            _ => LanguageSizeLimits {
                typical_lines: 250,
                large_lines: 500,
                huge_lines: 1000,
            },
        }
    }
}

/// Size thresholds for language files
#[derive(Debug, Clone, Copy)]
pub struct LanguageSizeLimits {
    pub typical_lines: usize,
    pub large_lines: usize,
    pub huge_lines: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_extension() {
        assert_eq!(Language::from_extension("rs"), Language::Rust);
        assert_eq!(Language::from_extension("RS"), Language::Rust);
        assert_eq!(Language::from_extension("py"), Language::Python);
        assert_eq!(Language::from_extension("js"), Language::JavaScript);
        assert_eq!(Language::from_extension("ts"), Language::TypeScript);
        assert_eq!(Language::from_extension("unknown"), Language::Unknown);
    }

    #[test]
    fn test_from_path() {
        assert_eq!(Language::from_path("test.rs"), Language::Rust);
        assert_eq!(Language::from_path("src/main.py"), Language::Python);
        assert_eq!(Language::from_path("index.ts"), Language::TypeScript);
        assert_eq!(Language::from_path("no_extension"), Language::Unknown);
    }

    #[test]
    fn test_supports_ast() {
        assert!(Language::Rust.supports_ast());
        assert!(Language::Python.supports_ast());
        assert!(Language::JavaScript.supports_ast());
        assert!(Language::TypeScript.supports_ast());
        assert!(!Language::Go.supports_ast());
        assert!(!Language::Unknown.supports_ast());
    }

    #[test]
    fn test_tree_sitter_language() {
        assert!(Language::Rust.tree_sitter_language().is_ok());
        assert!(Language::Python.tree_sitter_language().is_ok());
        assert!(Language::JavaScript.tree_sitter_language().is_ok());
        assert!(Language::TypeScript.tree_sitter_language().is_ok());
        assert!(Language::Go.tree_sitter_language().is_err());
    }

    #[test]
    fn test_comment_prefixes() {
        assert!(!Language::Rust.comment_prefixes().is_empty());
        assert!(Language::Rust.comment_prefixes().contains(&"//"));
        assert!(Language::Python.comment_prefixes().contains(&"#"));
    }

    #[test]
    fn test_import_patterns() {
        assert!(Language::Rust.import_patterns().contains(&"use "));
        assert!(Language::Python.import_patterns().contains(&"import "));
        assert!(Language::JavaScript.import_patterns().contains(&"import "));
    }
}
