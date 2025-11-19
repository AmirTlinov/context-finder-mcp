use anyhow::{Context as AnyhowContext, Result};
use clap::{Parser, Subcommand};
use context_code_chunker::{Chunker, ChunkerConfig};
use context_search::HybridSearch;
use context_vector_store::VectorStore;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "context-finder")]
#[command(about = "Semantic code search for AI models", long_about = None)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Index a project
    Index {
        /// Project path to index
        path: PathBuf,
    },

    /// Search indexed project
    Search {
        /// Search query
        query: String,

        /// Maximum results to return
        #[arg(short, long, default_value_t = 10)]
        limit: usize,

        /// Project path (defaults to current directory)
        #[arg(short, long)]
        project: Option<PathBuf>,
    },

    /// Get context around a specific line
    GetContext {
        /// File path (relative to project root)
        file: String,

        /// Line number
        line: usize,

        /// Context window (lines before/after)
        #[arg(short, long, default_value_t = 20)]
        window: usize,

        /// Project path (defaults to current directory)
        #[arg(short, long)]
        project: Option<PathBuf>,
    },

    /// List symbols in a file
    ListSymbols {
        /// File path (relative to project root)
        file: String,

        /// Project path (defaults to current directory)
        #[arg(short, long)]
        project: Option<PathBuf>,
    },
}

#[derive(Serialize, Deserialize)]
struct SearchOutput {
    query: String,
    results: Vec<SearchResultOutput>,
}

#[derive(Serialize, Deserialize)]
struct SearchResultOutput {
    file: String,
    start_line: usize,
    end_line: usize,
    symbol: Option<String>,
    #[serde(rename = "type")]
    chunk_type: Option<String>,
    score: f32,
    content: String,
    context: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct ContextOutput {
    file: String,
    line: usize,
    symbol: Option<String>,
    #[serde(rename = "type")]
    chunk_type: Option<String>,
    parent: Option<String>,
    imports: Vec<String>,
    content: String,
    window: WindowOutput,
}

#[derive(Serialize, Deserialize)]
struct WindowOutput {
    before: String,
    after: String,
}

#[derive(Serialize, Deserialize)]
struct SymbolsOutput {
    file: String,
    symbols: Vec<SymbolInfo>,
}

#[derive(Serialize, Deserialize)]
struct SymbolInfo {
    name: String,
    #[serde(rename = "type")]
    symbol_type: String,
    parent: Option<String>,
    line: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.verbose {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    }

    match cli.command {
        Commands::Index { path } => {
            cmd_index(&path).await?;
        }

        Commands::Search {
            query,
            limit,
            project,
        } => {
            let project = project.unwrap_or_else(|| PathBuf::from("."));
            cmd_search(&query, limit, &project).await?;
        }

        Commands::GetContext {
            file,
            line,
            window,
            project,
        } => {
            let project = project.unwrap_or_else(|| PathBuf::from("."));
            cmd_get_context(&file, line, window, &project).await?;
        }

        Commands::ListSymbols { file, project } => {
            let project = project.unwrap_or_else(|| PathBuf::from("."));
            cmd_list_symbols(&file, &project).await?;
        }
    }

    Ok(())
}

async fn cmd_index(path: &Path) -> Result<()> {
    let indexer = context_indexer::ProjectIndexer::new(path).await?;
    let stats = indexer.index().await?;

    println!("{}", serde_json::to_string_pretty(&stats)?);
    Ok(())
}

async fn cmd_search(query: &str, limit: usize, project: &Path) -> Result<()> {
    let store_path = project.join(".context-finder").join("index.json");

    if !store_path.exists() {
        anyhow::bail!("Index not found. Run 'context-finder index <path>' first.");
    }

    // Load store and chunks
    let store = VectorStore::load(&store_path)
        .await
        .context("Failed to load vector store")?;

    // Load all chunks from store
    let chunks: Vec<_> = store
        .chunk_ids()
        .into_iter()
        .filter_map(|id| store.get_chunk(&id).map(|sc| sc.chunk.clone()))
        .collect();

    log::info!("Loaded {} chunks from index", chunks.len());

    // Create hybrid search
    let mut search = HybridSearch::new(store, chunks)
        .await
        .context("Failed to create search engine")?;

    // Search
    let results = search
        .search(query, limit)
        .await
        .context("Search failed")?;

    // Format output
    let output = SearchOutput {
        query: query.to_string(),
        results: results
            .into_iter()
            .map(|r| SearchResultOutput {
                file: r.chunk.file_path.clone(),
                start_line: r.chunk.start_line,
                end_line: r.chunk.end_line,
                symbol: r.chunk.metadata.symbol_name.clone(),
                chunk_type: r.chunk.metadata.chunk_type.map(|ct| ct.as_str().to_string()),
                score: r.score,
                content: r.chunk.content.clone(),
                context: r.chunk.metadata.context_imports.clone(),
            })
            .collect(),
    };

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

async fn cmd_get_context(file: &str, line: usize, window: usize, project: &Path) -> Result<()> {
    let file_path = project.join(file);

    if !file_path.exists() {
        anyhow::bail!("File not found: {}", file_path.display());
    }

    let content = tokio::fs::read_to_string(&file_path)
        .await
        .context("Failed to read file")?;

    let lines: Vec<&str> = content.lines().collect();

    if line == 0 || line > lines.len() {
        anyhow::bail!("Line {} out of range (file has {} lines)", line, lines.len());
    }

    // Find chunk containing this line
    let chunker = Chunker::new(ChunkerConfig::for_embeddings());
    let chunks = chunker
        .chunk_str(&content, Some(file))
        .context("Failed to chunk file")?;

    let target_chunk = chunks.iter().find(|c| c.contains_line(line));

    // Get window
    let start = line.saturating_sub(window).max(1);
    let end = (line + window).min(lines.len());

    let before = lines[start.saturating_sub(1)..line.saturating_sub(1)].join("\n");
    let after = lines[line..end].join("\n");

    let output = ContextOutput {
        file: file.to_string(),
        line,
        symbol: target_chunk.and_then(|c| c.metadata.symbol_name.clone()),
        chunk_type: target_chunk
            .and_then(|c| c.metadata.chunk_type.map(|ct| ct.as_str().to_string())),
        parent: target_chunk.and_then(|c| c.metadata.parent_scope.clone()),
        imports: target_chunk
            .map(|c| c.metadata.context_imports.clone())
            .unwrap_or_default(),
        content: target_chunk.map(|c| c.content.clone()).unwrap_or_default(),
        window: WindowOutput { before, after },
    };

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

async fn cmd_list_symbols(file: &str, project: &Path) -> Result<()> {
    let file_path = project.join(file);

    if !file_path.exists() {
        anyhow::bail!("File not found: {}", file_path.display());
    }

    let content = tokio::fs::read_to_string(&file_path)
        .await
        .context("Failed to read file")?;

    let chunker = Chunker::new(ChunkerConfig::for_embeddings());
    let chunks = chunker
        .chunk_str(&content, Some(file))
        .context("Failed to chunk file")?;

    let symbols: Vec<SymbolInfo> = chunks
        .iter()
        .filter_map(|chunk| {
            let name = chunk.metadata.symbol_name.clone()?;
            let symbol_type = chunk
                .metadata
                .chunk_type
                .map(|ct| ct.as_str().to_string())
                .unwrap_or_else(|| "unknown".to_string());

            Some(SymbolInfo {
                name,
                symbol_type,
                parent: chunk.metadata.parent_scope.clone(),
                line: chunk.start_line,
            })
        })
        .collect();

    let output = SymbolsOutput {
        file: file.to_string(),
        symbols,
    };

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
