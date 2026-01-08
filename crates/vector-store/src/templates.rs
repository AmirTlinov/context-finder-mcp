use crate::error::{Result, VectorStoreError};
use context_code_chunker::CodeChunk;
use serde::{Deserialize, Serialize};

pub const EMBEDDING_TEMPLATES_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryKind {
    Identifier,
    Path,
    Conceptual,
}

impl QueryKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Identifier => "identifier",
            Self::Path => "path",
            Self::Conceptual => "conceptual",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    Code,
    Docs,
    Config,
    Test,
    Other,
}

impl DocumentKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Docs => "docs",
            Self::Config => "config",
            Self::Test => "test",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbeddingTemplates {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_max_chars")]
    pub max_chars: usize,
    #[serde(default)]
    pub query: QueryTemplates,
    #[serde(default)]
    pub document: DocumentTemplates,
    #[serde(default)]
    pub graph_node: GraphNodeTemplates,
}

const fn default_schema_version() -> u32 {
    EMBEDDING_TEMPLATES_SCHEMA_VERSION
}

const fn default_max_chars() -> usize {
    8192
}

impl Default for EmbeddingTemplates {
    fn default() -> Self {
        Self {
            schema_version: EMBEDDING_TEMPLATES_SCHEMA_VERSION,
            max_chars: default_max_chars(),
            query: QueryTemplates::default(),
            document: DocumentTemplates::default(),
            graph_node: GraphNodeTemplates::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryTemplates {
    #[serde(default = "default_template")]
    pub default: String,
    pub identifier: Option<String>,
    pub path: Option<String>,
    pub conceptual: Option<String>,
}

impl Default for QueryTemplates {
    fn default() -> Self {
        Self {
            default: default_template(),
            identifier: None,
            path: None,
            conceptual: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentTemplates {
    #[serde(default = "default_template")]
    pub default: String,
    pub code: Option<String>,
    pub docs: Option<String>,
    pub config: Option<String>,
    pub test: Option<String>,
}

impl Default for DocumentTemplates {
    fn default() -> Self {
        Self {
            default: default_template(),
            code: None,
            docs: None,
            config: None,
            test: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphNodeTemplates {
    #[serde(default = "default_template")]
    pub default: String,
}

impl Default for GraphNodeTemplates {
    fn default() -> Self {
        Self {
            default: default_template(),
        }
    }
}

fn default_template() -> String {
    "{text}".to_string()
}

impl EmbeddingTemplates {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != EMBEDDING_TEMPLATES_SCHEMA_VERSION {
            return Err(VectorStoreError::EmbeddingError(format!(
                "Unsupported embedding template schema_version {} (expected {EMBEDDING_TEMPLATES_SCHEMA_VERSION})",
                self.schema_version
            )));
        }

        let max_chars = self.max_chars;
        if !(256..=200_000).contains(&max_chars) {
            return Err(VectorStoreError::EmbeddingError(format!(
                "embedding.max_chars must be in [256, 200000] (got {max_chars})"
            )));
        }

        for template in self.all_templates() {
            validate_template_placeholders(template)?;
        }

        Ok(())
    }

    #[must_use]
    pub fn doc_template_hash(&self) -> u64 {
        use std::fmt::Write as _;

        let mut repr = String::new();

        let _ = writeln!(
            &mut repr,
            "schema_version={}\nmax_chars={}",
            self.schema_version, self.max_chars
        );
        let _ = writeln!(&mut repr, "doc.default={}", self.document.default);
        let _ = writeln!(
            &mut repr,
            "doc.code={}",
            self.document.code.as_deref().unwrap_or_default()
        );
        let _ = writeln!(
            &mut repr,
            "doc.docs={}",
            self.document.docs.as_deref().unwrap_or_default()
        );
        let _ = writeln!(
            &mut repr,
            "doc.config={}",
            self.document.config.as_deref().unwrap_or_default()
        );
        let _ = writeln!(
            &mut repr,
            "doc.test={}",
            self.document.test.as_deref().unwrap_or_default()
        );
        fnv1a64(repr.as_bytes())
    }

    #[must_use]
    pub fn graph_node_template_hash(&self) -> u64 {
        let repr = format!(
            "schema_version={}\nmax_chars={}\ngraph.default={}\n",
            self.schema_version, self.max_chars, self.graph_node.default
        );
        fnv1a64(repr.as_bytes())
    }

    pub fn render_query(&self, kind: QueryKind, text: &str) -> Result<String> {
        let template = match kind {
            QueryKind::Identifier => self.query.identifier.as_deref(),
            QueryKind::Path => self.query.path.as_deref(),
            QueryKind::Conceptual => self.query.conceptual.as_deref(),
        }
        .unwrap_or(self.query.default.as_str());

        render_template(template, self.max_chars, |key| match key {
            "text" => Some(text),
            "query_kind" => Some(kind.as_str()),
            _ => None,
        })
    }

    pub fn render_doc_chunk(&self, chunk: &CodeChunk) -> Result<String> {
        let kind = classify_document_kind(chunk);
        let template = match kind {
            DocumentKind::Code => self.document.code.as_deref(),
            DocumentKind::Docs => self.document.docs.as_deref(),
            DocumentKind::Config => self.document.config.as_deref(),
            DocumentKind::Test => self.document.test.as_deref(),
            DocumentKind::Other => None,
        }
        .unwrap_or(self.document.default.as_str());

        let chunk_type = chunk
            .metadata
            .chunk_type
            .map_or("", context_code_chunker::ChunkType::as_str);
        let language = chunk.metadata.language.as_deref().unwrap_or("");
        let symbol = chunk.metadata.symbol_name.as_deref().unwrap_or("");
        let qualified_name = chunk.metadata.qualified_name.as_deref().unwrap_or("");
        let parent_scope = chunk.metadata.parent_scope.as_deref().unwrap_or("");
        let documentation = chunk.metadata.documentation.as_deref().unwrap_or("");

        let mut tags = chunk.metadata.tags.clone();
        tags.sort();
        tags.dedup();
        let tags_joined = tags.join(", ");

        let mut bundle_tags = chunk.metadata.bundle_tags.clone();
        bundle_tags.sort();
        bundle_tags.dedup();
        let bundle_joined = bundle_tags.join(", ");

        let mut related = chunk.metadata.related_paths.clone();
        related.sort();
        related.dedup();
        let related_joined = related.join(", ");

        let mut imports = chunk.metadata.context_imports.clone();
        imports.sort();
        imports.dedup();
        let imports_joined = imports.join("\n");

        let chunk_id = format!(
            "{}:{}:{}",
            chunk.file_path, chunk.start_line, chunk.end_line
        );
        let start_line = chunk.start_line.to_string();
        let end_line = chunk.end_line.to_string();

        render_template(template, self.max_chars, |key| match key {
            "text" => Some(chunk.content.as_str()),
            "path" => Some(chunk.file_path.as_str()),
            "language" => Some(language),
            "chunk_type" => Some(chunk_type),
            "symbol" => Some(symbol),
            "qualified_name" => Some(qualified_name),
            "parent_scope" => Some(parent_scope),
            "documentation" => Some(documentation),
            "imports" => Some(imports_joined.as_str()),
            "tags" => Some(tags_joined.as_str()),
            "bundle_tags" => Some(bundle_joined.as_str()),
            "related_paths" => Some(related_joined.as_str()),
            "chunk_id" => Some(chunk_id.as_str()),
            "start_line" => Some(start_line.as_str()),
            "end_line" => Some(end_line.as_str()),
            "doc_kind" => Some(match kind {
                DocumentKind::Code => "code",
                DocumentKind::Docs => "docs",
                DocumentKind::Config => "config",
                DocumentKind::Test => "test",
                DocumentKind::Other => "other",
            }),
            _ => None,
        })
    }

    pub fn render_graph_node_doc(&self, graph_doc: &str) -> Result<String> {
        render_template(&self.graph_node.default, self.max_chars, |key| match key {
            "text" => Some(graph_doc),
            _ => None,
        })
    }

    fn all_templates(&self) -> Vec<&str> {
        let mut out = Vec::new();
        out.push(self.query.default.as_str());
        if let Some(v) = &self.query.identifier {
            out.push(v.as_str());
        }
        if let Some(v) = &self.query.path {
            out.push(v.as_str());
        }
        if let Some(v) = &self.query.conceptual {
            out.push(v.as_str());
        }

        out.push(self.document.default.as_str());
        if let Some(v) = &self.document.code {
            out.push(v.as_str());
        }
        if let Some(v) = &self.document.docs {
            out.push(v.as_str());
        }
        if let Some(v) = &self.document.config {
            out.push(v.as_str());
        }
        if let Some(v) = &self.document.test {
            out.push(v.as_str());
        }

        out.push(self.graph_node.default.as_str());
        out
    }
}

#[must_use]
pub fn classify_document_kind(chunk: &CodeChunk) -> DocumentKind {
    classify_path_kind(chunk.file_path.as_str())
}

#[must_use]
pub fn classify_path_kind(path: &str) -> DocumentKind {
    fn has_extension(ext: Option<&str>, candidates: &[&str]) -> bool {
        ext.is_some_and(|ext| {
            candidates
                .iter()
                .any(|candidate| ext.eq_ignore_ascii_case(candidate))
        })
    }

    let path = std::path::Path::new(path);
    let ext = path.extension().and_then(|e| e.to_str());

    if has_extension(ext, &["md", "mdx", "rst", "adoc", "txt", "context"]) {
        return DocumentKind::Docs;
    }
    if has_extension(ext, &["toml", "yaml", "yml", "json", "ini", "cfg", "conf"]) {
        return DocumentKind::Config;
    }
    if path.components().any(|component| match component {
        std::path::Component::Normal(s) => {
            let s = s.to_string_lossy();
            s.eq_ignore_ascii_case("test")
                || s.eq_ignore_ascii_case("tests")
                || s.eq_ignore_ascii_case("__tests__")
        }
        _ => false,
    }) {
        return DocumentKind::Test;
    }
    if ext.is_some() {
        return DocumentKind::Code;
    }
    DocumentKind::Other
}

fn validate_template_placeholders(template: &str) -> Result<()> {
    for name in extract_placeholders(template)? {
        if !is_allowed_placeholder(&name) {
            return Err(VectorStoreError::EmbeddingError(format!(
                "Unsupported template placeholder '{{{name}}}'. Allowed: {ALLOWED_PLACEHOLDERS}",
            )));
        }
    }
    Ok(())
}

const ALLOWED_PLACEHOLDERS: &str = "text, path, language, chunk_type, symbol, qualified_name, parent_scope, documentation, imports, tags, bundle_tags, related_paths, chunk_id, start_line, end_line, doc_kind, query_kind";

fn is_allowed_placeholder(name: &str) -> bool {
    matches!(
        name,
        "text"
            | "path"
            | "language"
            | "chunk_type"
            | "symbol"
            | "qualified_name"
            | "parent_scope"
            | "documentation"
            | "imports"
            | "tags"
            | "bundle_tags"
            | "related_paths"
            | "chunk_id"
            | "start_line"
            | "end_line"
            | "doc_kind"
            | "query_kind"
    )
}

fn extract_placeholders(template: &str) -> Result<Vec<String>> {
    let mut placeholders = Vec::new();
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' => {
                if matches!(chars.peek(), Some('{')) {
                    let _ = chars.next();
                    continue;
                }
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some('{') => {
                            return Err(VectorStoreError::EmbeddingError(
                                "Invalid template: nested '{' inside placeholder".into(),
                            ));
                        }
                        Some(c) => name.push(c),
                        None => {
                            return Err(VectorStoreError::EmbeddingError(
                                "Invalid template: unterminated '{...}' placeholder".into(),
                            ));
                        }
                    }
                }
                if name.trim().is_empty() {
                    return Err(VectorStoreError::EmbeddingError(
                        "Invalid template: empty placeholder '{}'".into(),
                    ));
                }
                placeholders.push(name);
            }
            '}' => {
                if matches!(chars.peek(), Some('}')) {
                    let _ = chars.next();
                    continue;
                }
                return Err(VectorStoreError::EmbeddingError(
                    "Invalid template: stray '}'".into(),
                ));
            }
            _ => {}
        }
    }
    Ok(placeholders)
}

fn render_template<'a>(
    template: &str,
    max_chars: usize,
    mut resolve: impl FnMut(&str) -> Option<&'a str>,
) -> Result<String> {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if out.len() >= max_chars {
            break;
        }

        match ch {
            '{' => {
                if matches!(chars.peek(), Some('{')) {
                    let _ = chars.next();
                    push_char_bounded(&mut out, '{', max_chars);
                    continue;
                }

                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some('{') => {
                            return Err(VectorStoreError::EmbeddingError(
                                "Invalid template: nested '{' inside placeholder".into(),
                            ));
                        }
                        Some(c) => name.push(c),
                        None => {
                            return Err(VectorStoreError::EmbeddingError(
                                "Invalid template: unterminated '{...}' placeholder".into(),
                            ));
                        }
                    }
                }

                if !is_allowed_placeholder(name.trim()) {
                    return Err(VectorStoreError::EmbeddingError(format!(
                        "Unsupported template placeholder '{{{}}}'. Allowed: {ALLOWED_PLACEHOLDERS}",
                        name.trim()
                    )));
                }

                let value = resolve(name.trim()).unwrap_or("");
                push_str_bounded(&mut out, value, max_chars);
            }
            '}' => {
                if matches!(chars.peek(), Some('}')) {
                    let _ = chars.next();
                    push_char_bounded(&mut out, '}', max_chars);
                    continue;
                }
                return Err(VectorStoreError::EmbeddingError(
                    "Invalid template: stray '}'".into(),
                ));
            }
            other => push_char_bounded(&mut out, other, max_chars),
        }
    }

    Ok(out)
}

fn push_char_bounded(out: &mut String, ch: char, max_chars: usize) {
    let remaining = max_chars.saturating_sub(out.len());
    if remaining == 0 {
        return;
    }
    let mut buf = [0u8; 4];
    let encoded = ch.encode_utf8(&mut buf);
    push_str_bounded(out, encoded, max_chars);
}

fn push_str_bounded(out: &mut String, value: &str, max_chars: usize) {
    let remaining = max_chars.saturating_sub(out.len());
    if remaining == 0 {
        return;
    }
    if value.len() <= remaining {
        out.push_str(value);
        return;
    }
    out.push_str(utf8_prefix(value, remaining));
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = 0;
    for (i, _) in value.char_indices() {
        if i > max_bytes {
            break;
        }
        end = i;
    }
    &value[..end]
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 14_695_981_039_346_656_037;
    const PRIME: u64 = 1_099_511_628_211;
    let mut hash = OFFSET;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}
