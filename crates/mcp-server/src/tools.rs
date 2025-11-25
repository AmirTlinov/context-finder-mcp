//! MCP Tools for Context Finder
//!
//! Provides semantic code search capabilities to AI agents via MCP protocol.

use anyhow::{Context as AnyhowContext, Result};
use context_graph::{GraphBuilder, GraphLanguage};
use context_search::{ContextSearch, HybridSearch, SearchProfile};
use context_vector_store::VectorStore;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo,
};
use rmcp::schemars;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Context Finder MCP Service
#[derive(Clone)]
pub struct ContextFinderService {
    /// Search profile
    profile: SearchProfile,
    /// Tool router
    tool_router: ToolRouter<Self>,
}

impl ContextFinderService {
    pub fn new() -> Self {
        Self {
            profile: SearchProfile::general(),
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_handler]
impl ServerHandler for ContextFinderService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some("Context Finder provides semantic code search for AI agents. Use 'map' to explore project structure, 'search' for semantic queries, 'context' for search with related code, and 'index' to index new projects.".into()),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            ..Default::default()
        }
    }
}

impl ContextFinderService {
    /// Get canonical path and index path, checking if index exists
    fn resolve_project(path: &PathBuf) -> Result<(PathBuf, PathBuf)> {
        let canonical = path.canonicalize().context("Invalid project path")?;
        let index_path = canonical.join(".context-finder/index.json");
        if !index_path.exists() {
            anyhow::bail!(
                "Index not found at {}. Run 'context-finder index' first.",
                index_path.display()
            );
        }
        Ok((canonical, index_path))
    }

    /// Load store and chunks from index path
    async fn load_store(index_path: &PathBuf) -> Result<(VectorStore, Vec<context_code_chunker::CodeChunk>)> {
        let store = VectorStore::load(index_path)
            .await
            .context("Failed to load vector store")?;

        let mut chunks = Vec::new();
        for id in store.chunk_ids() {
            if let Some(stored) = store.get_chunk(&id) {
                chunks.push(stored.chunk.clone());
            }
        }

        Ok((store, chunks))
    }
}

// ============================================================================
// Tool Input/Output Schemas
// ============================================================================

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MapRequest {
    /// Project directory path (defaults to current directory)
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Directory depth for aggregation (default: 2)
    #[schemars(description = "Directory depth for grouping (1-4)")]
    pub depth: Option<usize>,

    /// Maximum number of directories to return
    #[schemars(description = "Limit number of results")]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MapResult {
    /// Total files in project
    pub total_files: usize,
    /// Total code chunks indexed
    pub total_chunks: usize,
    /// Total lines of code
    pub total_lines: usize,
    /// Directory breakdown
    pub directories: Vec<DirectoryInfo>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DirectoryInfo {
    /// Directory path
    pub path: String,
    /// Number of files
    pub files: usize,
    /// Number of chunks
    pub chunks: usize,
    /// Percentage of codebase
    pub coverage_pct: f32,
    /// Top symbols in this directory
    pub top_symbols: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchRequest {
    /// Search query (semantic search)
    #[schemars(description = "Natural language search query")]
    pub query: String,

    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Maximum results (default: 10)
    #[schemars(description = "Maximum number of results (1-50)")]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SearchResult {
    /// File path
    pub file: String,
    /// Start line
    pub start_line: usize,
    /// End line
    pub end_line: usize,
    /// Symbol name (if any)
    pub symbol: Option<String>,
    /// Symbol type (function, struct, etc.)
    pub symbol_type: Option<String>,
    /// Relevance score (0-1)
    pub score: f32,
    /// Code content
    pub content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContextRequest {
    /// Search query
    #[schemars(description = "Natural language search query")]
    pub query: String,

    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Maximum primary results (default: 5)
    #[schemars(description = "Maximum number of primary results")]
    pub limit: Option<usize>,

    /// Search strategy: direct, extended, deep
    #[schemars(description = "Graph traversal depth: direct (none), extended (1-hop), deep (2-hop)")]
    pub strategy: Option<String>,

    /// Graph language: rust, python, javascript, typescript
    #[schemars(description = "Programming language for graph analysis")]
    pub language: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ContextResult {
    /// Primary search results
    pub results: Vec<ContextHit>,
    /// Total related code found
    pub related_count: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ContextHit {
    /// File path
    pub file: String,
    /// Start line
    pub start_line: usize,
    /// End line
    pub end_line: usize,
    /// Symbol name
    pub symbol: Option<String>,
    /// Relevance score
    pub score: f32,
    /// Code content
    pub content: String,
    /// Related code through graph
    pub related: Vec<RelatedCode>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RelatedCode {
    /// File path
    pub file: String,
    /// Line range as string
    pub lines: String,
    /// Symbol name
    pub symbol: Option<String>,
    /// Relationship path (e.g., "Calls", "Uses -> Uses")
    pub relationship: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IndexRequest {
    /// Project directory path
    #[schemars(description = "Project directory to index")]
    pub path: Option<String>,

    /// Force full reindex
    #[schemars(description = "Force full reindex ignoring cache")]
    pub force: Option<bool>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct IndexResult {
    /// Number of files indexed
    pub files: usize,
    /// Number of chunks created
    pub chunks: usize,
    /// Indexing time in milliseconds
    pub time_ms: u64,
    /// Index file path
    pub index_path: String,
}

// ============================================================================
// New Tool Schemas: impact, trace, explain, overview
// ============================================================================

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ImpactRequest {
    /// Symbol name to analyze
    #[schemars(description = "Symbol name to find usages of (e.g., 'VectorStore', 'search')")]
    pub symbol: String,

    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Depth of transitive usages (1=direct, 2=transitive)
    #[schemars(description = "Depth for transitive impact analysis (1-3)")]
    pub depth: Option<usize>,

    /// Programming language
    #[schemars(description = "Programming language: rust, python, javascript, typescript")]
    pub language: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ImpactResult {
    /// Symbol that was analyzed
    pub symbol: String,
    /// Definition location
    pub definition: Option<SymbolLocation>,
    /// Total usage count
    pub total_usages: usize,
    /// Number of files affected
    pub files_affected: usize,
    /// Direct usages
    pub direct: Vec<UsageInfo>,
    /// Transitive usages (if depth > 1)
    pub transitive: Vec<UsageInfo>,
    /// Related tests
    pub tests: Vec<String>,
    /// Is part of public API
    pub public_api: bool,
    /// Mermaid diagram
    pub mermaid: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SymbolLocation {
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct UsageInfo {
    pub file: String,
    pub line: usize,
    pub symbol: String,
    pub relationship: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TraceRequest {
    /// Start symbol
    #[schemars(description = "Starting symbol name")]
    pub from: String,

    /// End symbol
    #[schemars(description = "Target symbol name")]
    pub to: String,

    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Programming language
    #[schemars(description = "Programming language: rust, python, javascript, typescript")]
    pub language: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TraceResult {
    /// Whether path was found
    pub found: bool,
    /// Call chain path
    pub path: Vec<TraceStep>,
    /// Path depth
    pub depth: usize,
    /// Mermaid sequence diagram
    pub mermaid: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TraceStep {
    /// Symbol name
    pub symbol: String,
    /// File path
    pub file: String,
    /// Line number
    pub line: usize,
    /// Relationship to next step
    pub relationship: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExplainRequest {
    /// Symbol name to explain
    #[schemars(description = "Symbol name to get detailed information about")]
    pub symbol: String,

    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Programming language
    #[schemars(description = "Programming language: rust, python, javascript, typescript")]
    pub language: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExplainResult {
    /// Symbol name
    pub symbol: String,
    /// Symbol kind (function, struct, etc.)
    pub kind: String,
    /// File path
    pub file: String,
    /// Line number
    pub line: usize,
    /// Documentation (if available)
    pub documentation: Option<String>,
    /// Dependencies (what this symbol uses/calls)
    pub dependencies: Vec<String>,
    /// Dependents (what uses/calls this symbol)
    pub dependents: Vec<String>,
    /// Related tests
    pub tests: Vec<String>,
    /// Code content
    pub content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OverviewRequest {
    /// Project directory path
    #[schemars(description = "Project directory path")]
    pub path: Option<String>,

    /// Programming language
    #[schemars(description = "Programming language: rust, python, javascript, typescript")]
    pub language: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct OverviewResult {
    /// Project info
    pub project: ProjectInfo,
    /// Architecture layers
    pub layers: Vec<LayerInfo>,
    /// Entry points
    pub entry_points: Vec<String>,
    /// Key types (most connected)
    pub key_types: Vec<KeyTypeInfo>,
    /// Graph statistics
    pub graph_stats: GraphStats,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ProjectInfo {
    pub name: String,
    pub files: usize,
    pub chunks: usize,
    pub lines: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LayerInfo {
    pub name: String,
    pub files: usize,
    pub role: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct KeyTypeInfo {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub coupling: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GraphStats {
    pub nodes: usize,
    pub edges: usize,
}

// ============================================================================
// Tool Implementations
// ============================================================================

#[tool_router]
impl ContextFinderService {
    /// Get project structure overview
    #[tool(description = "Get project structure overview with directories, files, and top symbols. Use this first to understand a new codebase.")]
    pub async fn map(
        &self,
        Parameters(request): Parameters<MapRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let depth = request.depth.unwrap_or(2).clamp(1, 4);
        let limit = request.limit.unwrap_or(10);

        let (_root, index_path) = match Self::resolve_project(&path) {
            Ok(p) => p,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(format!("Error: {e}"))])),
        };

        let (_store, chunks) = match Self::load_store(&index_path).await {
            Ok(s) => s,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(format!("Error: {e}"))])),
        };

        // Aggregate by directory
        let mut tree_files: HashMap<String, HashSet<String>> = HashMap::new();
        let mut tree_chunks: HashMap<String, usize> = HashMap::new();
        let mut tree_symbols: HashMap<String, Vec<String>> = HashMap::new();
        let mut total_lines = 0usize;

        for chunk in &chunks {
            let parts: Vec<&str> = chunk.file_path.split('/').collect();
            let key = parts.iter().take(depth).cloned().collect::<Vec<_>>().join("/");

            tree_files.entry(key.clone()).or_default().insert(chunk.file_path.clone());
            *tree_chunks.entry(key.clone()).or_insert(0) += 1;
            total_lines += chunk.content.lines().count().max(1);

            if let Some(sym) = &chunk.metadata.symbol_name {
                let sym_type = chunk
                    .metadata
                    .chunk_type
                    .map(|ct| ct.as_str())
                    .unwrap_or("symbol");
                tree_symbols
                    .entry(key)
                    .or_default()
                    .push(format!("{} {}", sym_type, sym));
            }
        }

        let total_files: usize = tree_files.values().map(|s| s.len()).sum();
        let total_chunks = chunks.len();

        let mut directories: Vec<DirectoryInfo> = tree_chunks
            .into_iter()
            .map(|(path, chunks)| {
                let files = tree_files.get(&path).map(|s| s.len()).unwrap_or(0);
                let coverage_pct = if total_chunks > 0 {
                    chunks as f32 / total_chunks as f32 * 100.0
                } else {
                    0.0
                };
                let top_symbols: Vec<String> = tree_symbols
                    .get(&path)
                    .map(|v| v.iter().take(5).cloned().collect())
                    .unwrap_or_default();

                DirectoryInfo {
                    path,
                    files,
                    chunks,
                    coverage_pct,
                    top_symbols,
                }
            })
            .collect();

        directories.sort_by(|a, b| b.chunks.cmp(&a.chunks));
        directories.truncate(limit);

        let result = MapResult {
            total_files,
            total_chunks,
            total_lines,
            directories,
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Semantic code search
    #[tool(description = "Search for code using natural language. Returns relevant code snippets with file locations and symbols.")]
    pub async fn search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let limit = request.limit.unwrap_or(10).clamp(1, 50);

        if request.query.trim().is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "Error: Query cannot be empty",
            )]));
        }

        let (_root, index_path) = match Self::resolve_project(&path) {
            Ok(p) => p,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(format!("Error: {e}"))])),
        };

        let (store, chunks) = match Self::load_store(&index_path).await {
            Ok(s) => s,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(format!("Error: {e}"))])),
        };

        let mut search = match HybridSearch::with_profile(store, chunks, self.profile.clone()) {
            Ok(s) => s,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error creating search: {e}"
                ))]));
            }
        };

        let results = match search.search(&request.query, limit).await {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Search error: {e}"
                ))]));
            }
        };

        let formatted: Vec<SearchResult> = results
            .into_iter()
            .map(|r| SearchResult {
                file: r.chunk.file_path.clone(),
                start_line: r.chunk.start_line,
                end_line: r.chunk.end_line,
                symbol: r.chunk.metadata.symbol_name.clone(),
                symbol_type: r.chunk.metadata.chunk_type.map(|ct| ct.as_str().to_string()),
                score: r.score,
                content: r.chunk.content.clone(),
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&formatted).unwrap_or_default(),
        )]))
    }

    /// Search with graph context
    #[tool(description = "Search for code with automatic graph-based context. Returns code plus related functions/types through call graphs and dependencies. Best for understanding how code connects.")]
    pub async fn context(
        &self,
        Parameters(request): Parameters<ContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let limit = request.limit.unwrap_or(5).clamp(1, 20);
        let strategy = match request.strategy.as_deref() {
            Some("direct") => context_graph::AssemblyStrategy::Direct,
            Some("deep") => context_graph::AssemblyStrategy::Deep,
            _ => context_graph::AssemblyStrategy::Extended,
        };
        let language = match request.language.as_deref() {
            Some("python") => GraphLanguage::Python,
            Some("javascript") => GraphLanguage::JavaScript,
            Some("typescript") => GraphLanguage::TypeScript,
            _ => GraphLanguage::Rust,
        };

        if request.query.trim().is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "Error: Query cannot be empty",
            )]));
        }

        let (_root, index_path) = match Self::resolve_project(&path) {
            Ok(p) => p,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(format!("Error: {e}"))])),
        };

        let (store, chunks) = match Self::load_store(&index_path).await {
            Ok(s) => s,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(format!("Error: {e}"))])),
        };

        let hybrid = match HybridSearch::with_profile(store, chunks, self.profile.clone()) {
            Ok(s) => s,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {e}"
                ))]));
            }
        };

        let mut context_search = match ContextSearch::new(hybrid) {
            Ok(s) => s,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {e}"
                ))]));
            }
        };

        if let Err(e) = context_search.build_graph(language) {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Graph build error: {e}"
            ))]));
        }

        let enriched = match context_search
            .search_with_context(&request.query, limit, strategy)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Search error: {e}"
                ))]));
            }
        };

        let mut related_count = 0;
        let results: Vec<ContextHit> = enriched
            .into_iter()
            .map(|er| {
                let related: Vec<RelatedCode> = er
                    .related
                    .iter()
                    .take(5)
                    .map(|rc| {
                        related_count += 1;
                        RelatedCode {
                            file: rc.chunk.file_path.clone(),
                            lines: format!("{}-{}", rc.chunk.start_line, rc.chunk.end_line),
                            symbol: rc.chunk.metadata.symbol_name.clone(),
                            relationship: rc.relationship_path.join(" -> "),
                        }
                    })
                    .collect();

                ContextHit {
                    file: er.primary.chunk.file_path.clone(),
                    start_line: er.primary.chunk.start_line,
                    end_line: er.primary.chunk.end_line,
                    symbol: er.primary.chunk.metadata.symbol_name.clone(),
                    score: er.primary.score,
                    content: er.primary.chunk.content.clone(),
                    related,
                }
            })
            .collect();

        let result = ContextResult {
            results,
            related_count,
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Index a project
    #[tool(description = "Index a project directory for semantic search. Required before using search/context tools on a new project.")]
    pub async fn index(
        &self,
        Parameters(request): Parameters<IndexRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let force = request.force.unwrap_or(false);

        let canonical = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid path: {e}"
                ))]));
            }
        };

        let start = std::time::Instant::now();

        // Use indexer
        let indexer = match context_indexer::ProjectIndexer::new(&canonical).await {
            Ok(i) => i,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Indexer init error: {e}"
                ))]));
            }
        };

        let stats = if force {
            match indexer.index_full().await {
                Ok(s) => s,
                Err(e) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Indexing error: {e}"
                    ))]));
                }
            }
        } else {
            match indexer.index().await {
                Ok(s) => s,
                Err(e) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Indexing error: {e}"
                    ))]));
                }
            }
        };

        let time_ms = start.elapsed().as_millis() as u64;
        let index_path = canonical.join(".context-finder/index.json");

        let result = IndexResult {
            files: stats.files,
            chunks: stats.chunks,
            time_ms,
            index_path: index_path.to_string_lossy().to_string(),
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Find all usages of a symbol (impact analysis)
    #[tool(description = "Find all places where a symbol is used. Essential for refactoring - shows direct usages, transitive dependencies, and related tests.")]
    pub async fn impact(
        &self,
        Parameters(request): Parameters<ImpactRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let depth = request.depth.unwrap_or(2).clamp(1, 3);
        let language = Self::parse_language(request.language.as_deref());

        let (_root, index_path) = match Self::resolve_project(&path) {
            Ok(p) => p,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(format!("Error: {e}"))])),
        };

        let (_store, chunks) = match Self::load_store(&index_path).await {
            Ok(s) => s,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(format!("Error: {e}"))])),
        };

        // Build graph
        let mut builder = match GraphBuilder::new(language) {
            Ok(b) => b,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Graph builder error: {e}"
                ))]));
            }
        };
        let graph = match builder.build(&chunks) {
            Ok(g) => g,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Graph build error: {e}"
                ))]));
            }
        };

        // Find the symbol node
        let node = match graph.find_node(&request.symbol) {
            Some(n) => n,
            None => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Symbol '{}' not found in graph",
                    request.symbol
                ))]));
            }
        };

        // Get definition location
        let definition = graph.get_node(node).map(|nd| SymbolLocation {
            file: nd.symbol.file_path.clone(),
            line: nd.symbol.start_line,
        });

        // Get direct usages (filter unknown symbols and markdown files)
        let direct_usages = graph.get_all_usages(node);
        let mut seen_direct: HashSet<(String, usize)> = HashSet::new();
        let direct: Vec<UsageInfo> = direct_usages
            .iter()
            .filter_map(|(n, rel)| {
                graph.get_node(*n).and_then(|nd| {
                    // Skip unknown symbols and markdown files
                    if nd.symbol.name == "unknown" || nd.symbol.file_path.ends_with(".md") {
                        return None;
                    }
                    // Deduplicate by (file, line)
                    let key = (nd.symbol.file_path.clone(), nd.symbol.start_line);
                    if seen_direct.contains(&key) {
                        return None;
                    }
                    seen_direct.insert(key);
                    Some(UsageInfo {
                        file: nd.symbol.file_path.clone(),
                        line: nd.symbol.start_line,
                        symbol: nd.symbol.name.clone(),
                        relationship: format!("{:?}", rel),
                    })
                })
            })
            .collect();

        // Get transitive usages if depth > 1
        let transitive_usages = if depth > 1 {
            graph.get_transitive_usages(node, depth)
        } else {
            vec![]
        };
        let mut seen_transitive: HashSet<(String, usize)> = HashSet::new();
        let transitive: Vec<UsageInfo> = transitive_usages
            .iter()
            .filter(|(_, d, _)| *d > 1)
            .filter_map(|(n, _, path)| {
                graph.get_node(*n).and_then(|nd| {
                    // Skip unknown symbols and markdown files
                    if nd.symbol.name == "unknown" || nd.symbol.file_path.ends_with(".md") {
                        return None;
                    }
                    // Deduplicate by (file, line)
                    let key = (nd.symbol.file_path.clone(), nd.symbol.start_line);
                    if seen_transitive.contains(&key) {
                        return None;
                    }
                    seen_transitive.insert(key);
                    Some(UsageInfo {
                        file: nd.symbol.file_path.clone(),
                        line: nd.symbol.start_line,
                        symbol: nd.symbol.name.clone(),
                        relationship: path
                            .iter()
                            .map(|r| format!("{:?}", r))
                            .collect::<Vec<_>>()
                            .join(" -> "),
                    })
                })
            })
            .collect();

        // Find related tests (deduplicated)
        let test_nodes = graph.find_related_tests(node);
        let mut tests: Vec<String> = test_nodes
            .iter()
            .filter_map(|n| {
                graph
                    .get_node(*n)
                    .map(|nd| format!("{}:{}", nd.symbol.file_path, nd.symbol.start_line))
            })
            .collect();
        tests.sort();
        tests.dedup();

        // Check if public API
        let public_api = graph.is_public_api(node);

        // Generate Mermaid diagram
        let mermaid = Self::generate_impact_mermaid(&request.symbol, &direct, &transitive);

        let total_usages = direct.len() + transitive.len();
        
        // Count unique files affected
        let files_affected: HashSet<&str> = direct
            .iter()
            .chain(transitive.iter())
            .map(|u| u.file.as_str())
            .collect();

        let result = ImpactResult {
            symbol: request.symbol,
            definition,
            total_usages,
            files_affected: files_affected.len(),
            direct,
            transitive,
            tests,
            public_api,
            mermaid,
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Trace call path between two symbols
    #[tool(description = "Show call chain from one symbol to another. Essential for understanding code flow and debugging.")]
    pub async fn trace(
        &self,
        Parameters(request): Parameters<TraceRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let language = Self::parse_language(request.language.as_deref());

        let (_root, index_path) = match Self::resolve_project(&path) {
            Ok(p) => p,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(format!("Error: {e}"))])),
        };

        let (_store, chunks) = match Self::load_store(&index_path).await {
            Ok(s) => s,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(format!("Error: {e}"))])),
        };

        // Build graph
        let mut builder = match GraphBuilder::new(language) {
            Ok(b) => b,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Graph builder error: {e}"
                ))]));
            }
        };
        let graph = match builder.build(&chunks) {
            Ok(g) => g,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Graph build error: {e}"
                ))]));
            }
        };

        // Find both symbols
        let from_node = match graph.find_node(&request.from) {
            Some(n) => n,
            None => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Symbol '{}' not found",
                    request.from
                ))]));
            }
        };

        let to_node = match graph.find_node(&request.to) {
            Some(n) => n,
            None => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Symbol '{}' not found",
                    request.to
                ))]));
            }
        };

        // Find path
        let path_with_edges = graph.find_path_with_edges(from_node, to_node);

        let (found, path_steps, depth) = match path_with_edges {
            Some(path) => {
                let steps: Vec<TraceStep> = path
                    .iter()
                    .map(|(n, rel)| {
                        let node_data = graph.get_node(*n);
                        TraceStep {
                            symbol: node_data
                                .map(|nd| nd.symbol.name.clone())
                                .unwrap_or_default(),
                            file: node_data
                                .map(|nd| nd.symbol.file_path.clone())
                                .unwrap_or_default(),
                            line: node_data.map(|nd| nd.symbol.start_line).unwrap_or(0),
                            relationship: rel.map(|r| format!("{:?}", r)),
                        }
                    })
                    .collect();
                let depth = steps.len().saturating_sub(1);
                (true, steps, depth)
            }
            None => (false, vec![], 0),
        };

        // Generate Mermaid sequence diagram
        let mermaid = Self::generate_trace_mermaid(&path_steps);

        let result = TraceResult {
            found,
            path: path_steps,
            depth,
            mermaid,
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Deep dive into a symbol
    #[tool(description = "Get complete information about a symbol: definition, dependencies, dependents, tests, and documentation.")]
    pub async fn explain(
        &self,
        Parameters(request): Parameters<ExplainRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));
        let language = Self::parse_language(request.language.as_deref());

        let (_root, index_path) = match Self::resolve_project(&path) {
            Ok(p) => p,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(format!("Error: {e}"))])),
        };

        let (_store, chunks) = match Self::load_store(&index_path).await {
            Ok(s) => s,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(format!("Error: {e}"))])),
        };

        // Build graph
        let mut builder = match GraphBuilder::new(language) {
            Ok(b) => b,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Graph builder error: {e}"
                ))]));
            }
        };
        let graph = match builder.build(&chunks) {
            Ok(g) => g,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Graph build error: {e}"
                ))]));
            }
        };

        // Find the symbol
        let node = match graph.find_node(&request.symbol) {
            Some(n) => n,
            None => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Symbol '{}' not found",
                    request.symbol
                ))]));
            }
        };

        let node_data = graph.get_node(node);
        let (deps, dependents_raw) = graph.get_symbol_relations(node);

        // Format dependencies (filter unknown and markdown, deduplicate)
        let mut dependencies: Vec<String> = deps
            .iter()
            .filter_map(|(n, rel)| {
                graph.get_node(*n).and_then(|nd| {
                    if nd.symbol.name == "unknown" || nd.symbol.file_path.ends_with(".md") {
                        return None;
                    }
                    Some(format!("{} ({:?})", nd.symbol.name, rel))
                })
            })
            .collect();
        dependencies.sort();
        dependencies.dedup();

        // Format dependents (filter unknown and markdown, deduplicate)
        let mut dependents: Vec<String> = dependents_raw
            .iter()
            .filter_map(|(n, rel)| {
                graph.get_node(*n).and_then(|nd| {
                    if nd.symbol.name == "unknown" || nd.symbol.file_path.ends_with(".md") {
                        return None;
                    }
                    Some(format!("{} ({:?})", nd.symbol.name, rel))
                })
            })
            .collect();
        dependents.sort();
        dependents.dedup();

        // Find tests (deduplicated)
        let test_nodes = graph.find_related_tests(node);
        let mut tests: Vec<String> = test_nodes
            .iter()
            .filter_map(|n| graph.get_node(*n).map(|nd| nd.symbol.name.clone()))
            .collect();
        tests.sort();
        tests.dedup();

        // Get symbol info
        let (kind, file, line, documentation, content) = match node_data {
            Some(nd) => {
                let doc = nd
                    .chunk
                    .as_ref()
                    .and_then(|c| c.metadata.documentation.clone());
                let content = nd
                    .chunk
                    .as_ref()
                    .map(|c| c.content.clone())
                    .unwrap_or_default();
                (
                    format!("{:?}", nd.symbol.symbol_type),
                    nd.symbol.file_path.clone(),
                    nd.symbol.start_line,
                    doc,
                    content,
                )
            }
            None => (String::new(), String::new(), 0, None, String::new()),
        };

        let result = ExplainResult {
            symbol: request.symbol,
            kind,
            file,
            line,
            documentation,
            dependencies,
            dependents,
            tests,
            content,
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Project architecture overview
    #[tool(description = "Get project architecture snapshot: layers, entry points, key types, and graph statistics. Use this first to understand a new codebase.")]
    pub async fn overview(
        &self,
        Parameters(request): Parameters<OverviewRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = PathBuf::from(request.path.unwrap_or_else(|| ".".to_string()));

        let (root, index_path) = match Self::resolve_project(&path) {
            Ok(p) => p,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(format!("Error: {e}"))])),
        };

        let (_store, chunks) = match Self::load_store(&index_path).await {
            Ok(s) => s,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(format!("Error: {e}"))])),
        };

        // Auto-detect language if not specified
        let language = match request.language.as_deref() {
            Some(lang) => Self::parse_language(Some(lang)),
            None => Self::detect_language(&chunks),
        };

        // Compute project info
        let total_files: HashSet<&str> = chunks.iter().map(|c| c.file_path.as_str()).collect();
        let total_lines: usize = chunks.iter().map(|c| c.content.lines().count()).sum();
        let project_name = root
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let project = ProjectInfo {
            name: project_name,
            files: total_files.len(),
            chunks: chunks.len(),
            lines: total_lines,
        };

        // Compute layers by top-level directory only (skip individual files at root)
        let mut layer_files: HashMap<String, HashSet<&str>> = HashMap::new();
        for chunk in &chunks {
            let parts: Vec<&str> = chunk.file_path.split('/').collect();
            // Only use directories (skip root-level files)
            if parts.len() > 1 {
                let layer = parts.first().copied().unwrap_or("root").to_string();
                layer_files
                    .entry(layer)
                    .or_default()
                    .insert(&chunk.file_path);
            }
        }

        let mut layers: Vec<LayerInfo> = layer_files
            .into_iter()
            .map(|(name, files)| {
                let role = Self::guess_layer_role(&name);
                LayerInfo {
                    name,
                    files: files.len(),
                    role,
                }
            })
            .collect();
        // Sort by file count descending for better overview
        layers.sort_by(|a, b| b.files.cmp(&a.files));

        // Build graph for analysis
        let mut builder = match GraphBuilder::new(language) {
            Ok(b) => b,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Graph builder error: {e}"
                ))]));
            }
        };
        let graph = match builder.build(&chunks) {
            Ok(g) => g,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Graph build error: {e}"
                ))]));
            }
        };

        // Find entry points (filter unknown, tests, and deduplicate)
        let entry_nodes = graph.find_entry_points();
        let mut entry_points: Vec<String> = entry_nodes
            .iter()
            .filter_map(|n| {
                graph.get_node(*n).and_then(|nd| {
                    let name = &nd.symbol.name;
                    // Skip unknown, test functions, and markdown
                    if name == "unknown" 
                        || name.starts_with("test_") 
                        || nd.symbol.file_path.ends_with(".md")
                        || nd.symbol.file_path.contains("/tests/")
                    {
                        return None;
                    }
                    Some(name.clone())
                })
            })
            .collect();
        entry_points.sort();
        entry_points.dedup();
        entry_points.truncate(10);

        // Find key types (hotspots) - filter tests and deduplicate
        let hotspots = graph.find_hotspots(20); // Get more to filter
        let mut seen_names: HashSet<String> = HashSet::new();
        let key_types: Vec<KeyTypeInfo> = hotspots
            .iter()
            .filter_map(|(n, coupling)| {
                graph.get_node(*n).and_then(|nd| {
                    let name = &nd.symbol.name;
                    // Skip tests, unknown, duplicates
                    if name == "unknown"
                        || name == "tests"
                        || name.starts_with("test_")
                        || nd.symbol.file_path.contains("/tests/")
                        || seen_names.contains(name)
                    {
                        return None;
                    }
                    seen_names.insert(name.clone());
                    Some(KeyTypeInfo {
                        name: name.clone(),
                        kind: format!("{:?}", nd.symbol.symbol_type),
                        file: nd.symbol.file_path.clone(),
                        coupling: *coupling,
                    })
                })
            })
            .take(10)
            .collect();

        // Graph stats
        let (nodes, edges) = graph.stats();
        let graph_stats = GraphStats { nodes, edges };

        let result = OverviewResult {
            project,
            layers,
            entry_points,
            key_types,
            graph_stats,
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }
}

// ============================================================================
// Helper functions
// ============================================================================

impl ContextFinderService {
    fn parse_language(lang: Option<&str>) -> GraphLanguage {
        match lang {
            Some("python") => GraphLanguage::Python,
            Some("javascript") => GraphLanguage::JavaScript,
            Some("typescript") => GraphLanguage::TypeScript,
            _ => GraphLanguage::Rust,
        }
    }

    /// Auto-detect primary language from file extensions in chunks
    fn detect_language(chunks: &[context_code_chunker::CodeChunk]) -> GraphLanguage {
        let mut rust_count = 0;
        let mut python_count = 0;
        let mut js_count = 0;
        let mut ts_count = 0;

        for chunk in chunks {
            if chunk.file_path.ends_with(".rs") {
                rust_count += 1;
            } else if chunk.file_path.ends_with(".py") {
                python_count += 1;
            } else if chunk.file_path.ends_with(".ts") || chunk.file_path.ends_with(".tsx") {
                ts_count += 1;
            } else if chunk.file_path.ends_with(".js") || chunk.file_path.ends_with(".jsx") {
                js_count += 1;
            }
        }

        let max = rust_count.max(python_count).max(js_count).max(ts_count);
        if max == 0 {
            return GraphLanguage::Rust; // default
        }
        if max == rust_count {
            GraphLanguage::Rust
        } else if max == python_count {
            GraphLanguage::Python
        } else if max == ts_count {
            GraphLanguage::TypeScript
        } else {
            GraphLanguage::JavaScript
        }
    }

    fn guess_layer_role(name: &str) -> String {
        match name.to_lowercase().as_str() {
            "cli" | "cmd" | "bin" => "Command-line interface".to_string(),
            "api" | "server" | "web" => "API/Server layer".to_string(),
            "core" | "lib" | "src" => "Core library".to_string(),
            "test" | "tests" => "Test suite".to_string(),
            "crates" => "Workspace crates".to_string(),
            "docs" | "doc" => "Documentation".to_string(),
            _ => "Module".to_string(),
        }
    }

    fn generate_impact_mermaid(symbol: &str, direct: &[UsageInfo], transitive: &[UsageInfo]) -> String {
        let mut lines = vec!["graph LR".to_string()];

        // Add direct edges
        for usage in direct.iter().take(10) {
            lines.push(format!(
                "    {}-->|{}|{}",
                Self::mermaid_safe(&usage.symbol),
                usage.relationship,
                Self::mermaid_safe(symbol)
            ));
        }

        // Add transitive edges (simplified)
        for usage in transitive.iter().take(5) {
            lines.push(format!(
                "    {}-.->|transitive|{}",
                Self::mermaid_safe(&usage.symbol),
                Self::mermaid_safe(symbol)
            ));
        }

        lines.join("\n")
    }

    fn generate_trace_mermaid(steps: &[TraceStep]) -> String {
        if steps.is_empty() {
            return "sequenceDiagram\n    Note over A: No path found".to_string();
        }

        let mut lines = vec!["sequenceDiagram".to_string()];

        for window in steps.windows(2) {
            let from = &window[0];
            let to = &window[1];
            let rel = to.relationship.as_deref().unwrap_or("calls");
            lines.push(format!(
                "    {}->>{}+: {}",
                Self::mermaid_safe(&from.symbol),
                Self::mermaid_safe(&to.symbol),
                rel
            ));
        }

        lines.join("\n")
    }

    fn mermaid_safe(s: &str) -> String {
        s.replace("::", "_")
            .replace('<', "_")
            .replace('>', "_")
            .replace(' ', "_")
    }
}
