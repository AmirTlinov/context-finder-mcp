use anyhow::{Context as AnyhowContext, Result};
use axum::{
    body::Body,
    http::{Response as HttpResponse, StatusCode},
    response::Response,
    routing::{get, post},
    Router,
};
use cache::{CacheBackend, CacheConfig};
use clap::{Args, Parser, Subcommand, ValueEnum};
use command::{
    classify_error, CommandAction, CommandRequest, CommandResponse, ContextPackOutput,
    ContextPackPayload, EvalCacheMode, EvalCompareOutput, EvalComparePayload, EvalOutput,
    EvalPayload, IndexPayload, IndexResponse, ListSymbolsPayload, MapOutput, MapPayload,
    SearchOutput, SearchPayload, SearchStrategy, SearchWithContextPayload, SymbolsOutput,
};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tonic::transport::Server;

use crate::command::infra::HealthPort;

mod cache;
mod command;
mod graph_cache;
mod grpc;
mod heartbeat;
mod models;
mod report;

#[derive(Parser)]
#[command(name = "context-finder")]
#[command(about = "Semantic code search for AI agents", long_about = None)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Quiet mode: log only warnings/errors (stdout is reserved for JSON)
    #[arg(long, global = true)]
    quiet: bool,

    /// Override embedding backend in this process
    #[arg(long, global = true, value_enum)]
    embed_mode: Option<EmbedMode>,

    /// Override embedding model id
    #[arg(long, global = true)]
    embed_model: Option<String>,

    /// Model cache directory (overrides CONTEXT_FINDER_MODEL_DIR)
    #[arg(long, global = true)]
    model_dir: Option<PathBuf>,

    /// CUDA device id
    #[arg(long, global = true)]
    cuda_device: Option<i32>,

    /// CUDA memory arena limit (MB)
    #[arg(long, global = true)]
    cuda_mem_limit_mb: Option<usize>,

    /// Cache directory for compare_search and heavy ops
    #[arg(long, global = true, default_value = ".context-finder/cache")]
    cache_dir: String,

    /// Cache TTL in seconds
    #[arg(long, global = true, default_value_t = 86_400)]
    cache_ttl_seconds: u64,

    /// Cache backend: file|memory
    #[arg(long, global = true, default_value = "file")]
    cache_backend: String,

    /// Profile for search heuristics (default: quality)
    #[arg(long, global = true)]
    profile: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Execute a JSON Command API request
    Command(CommandArgs),

    /// Index a project directory for semantic search
    Index(IndexArgs),

    /// Search for code matching a query
    Search(SearchArgs),

    /// Get context for code understanding
    #[command(name = "get-context")]
    GetContext(GetContextArgs),

    /// List symbols in the indexed project
    #[command(name = "list-symbols")]
    ListSymbols(ListSymbolsArgs),

    /// Run background daemon that keeps indexes warm for pinged projects
    #[command(name = "daemon-loop")]
    DaemonLoop(DaemonArgs),

    /// Serve Command API over HTTP (POST /command)
    ServeHttp(ServeArgs),

    /// Serve Command API over gRPC (tonic)
    ServeGrpc(ServeGrpcArgs),

    /// Show project structure overview (directories, files, top symbols)
    Map(MapArgs),

    /// Search with automatic graph context (best for AI agents)
    Context(ContextArgs),

    /// Produce a single bounded context pack (best default for agents)
    #[command(name = "context-pack")]
    ContextPack(ContextPackArgs),

    /// Install embedding model assets into the local model dir (default: ./models)
    #[command(name = "install-models")]
    InstallModels(InstallModelsArgs),

    /// Diagnose GPU/runtime + model installation
    Doctor(DoctorArgs),

    /// Evaluate retrieval quality on a golden dataset
    Eval(EvalArgs),

    /// Compare two profiles/model sets on a golden dataset (A/B)
    #[command(name = "eval-compare")]
    EvalCompare(EvalCompareArgs),
}

#[derive(Args)]
struct CommandArgs {
    /// Inline JSON payload (mutually exclusive with --file)
    #[arg(long, conflicts_with = "file")]
    json: Option<String>,

    /// Path to file containing JSON payload
    #[arg(long)]
    file: Option<PathBuf>,

    /// Pretty-print JSON response
    #[arg(long)]
    pretty: bool,

    /// Quiet mode (only warn/error logs to stderr; stdout remains pure JSON)
    #[arg(long, global = true)]
    quiet: bool,
}

#[derive(Args)]
struct DaemonArgs {
    /// Unix socket path for daemon IPC
    #[arg(long)]
    socket: Option<PathBuf>,
}

#[derive(Args)]
struct ServeArgs {
    /// Bind address, e.g. 127.0.0.1:7700
    #[arg(long, default_value = "127.0.0.1:7700")]
    bind: String,

    /// Cache backend: file|memory
    #[arg(long, default_value = "file")]
    cache_backend: String,
}

#[derive(Args)]
struct ServeGrpcArgs {
    /// Bind address, e.g. 127.0.0.1:50051
    #[arg(long, default_value = "127.0.0.1:50051")]
    bind: String,
}

#[derive(Args)]
struct IndexArgs {
    /// Project directory to index (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Force full reindex (ignore incremental cache)
    #[arg(long)]
    force: bool,

    /// Index all models referenced by the selected profile experts
    #[arg(long)]
    experts: bool,

    /// Additional embedding model ids to index (comma-separated)
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    models: Vec<String>,

    /// Output JSON format
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct SearchArgs {
    /// Search query
    query: String,

    /// Project directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    path: PathBuf,

    /// Maximum number of results
    #[arg(long, short = 'n', default_value_t = 10)]
    limit: usize,

    /// Include graph relations in results
    #[arg(long)]
    with_graph: bool,

    /// Output JSON format
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct GetContextArgs {
    /// Search queries for context gathering
    queries: Vec<String>,

    /// Project directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    path: PathBuf,

    /// Maximum number of results per query
    #[arg(long, short = 'n', default_value_t = 10)]
    limit: usize,

    /// Output JSON format
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct MapArgs {
    /// Project directory (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Directory depth for aggregation (default: 2)
    #[arg(long, short = 'd', default_value_t = 2)]
    depth: usize,

    /// Maximum number of top-level directories to show
    #[arg(long, short = 'n')]
    limit: Option<usize>,

    /// Output JSON format
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ContextArgs {
    /// Search query
    query: String,

    /// Project directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    path: PathBuf,

    /// Maximum number of results
    #[arg(long, short = 'n', default_value_t = 10)]
    limit: usize,

    /// Search strategy: direct, extended (default), deep
    #[arg(long, short = 's')]
    strategy: Option<String>,

    /// Include graph relationships in output
    #[arg(long)]
    show_graph: bool,

    /// Graph language: rust (default), python, javascript, typescript
    #[arg(long, short = 'l')]
    language: Option<String>,

    /// Output JSON format
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ContextPackArgs {
    /// Search query
    query: String,

    /// Project directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    path: PathBuf,

    /// Maximum number of primary results
    #[arg(long, short = 'n', default_value_t = 10)]
    limit: usize,

    /// Search strategy: direct, extended (default), deep
    #[arg(long, short = 's')]
    strategy: Option<String>,

    /// Maximum total chars across all packed items
    #[arg(long)]
    max_chars: Option<usize>,

    /// Related chunks per primary (graph halo cap)
    #[arg(long)]
    max_related_per_primary: Option<usize>,

    /// Graph language: rust (default), python, javascript, typescript
    #[arg(long, short = 'l')]
    language: Option<String>,

    /// Include debug hints and trace output in the JSON CommandResponse
    #[arg(long)]
    trace: bool,

    /// Output JSON format
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct InstallModelsArgs {
    /// Model ids to install (comma-separated). If omitted, installs all from manifest.
    #[arg(long, value_delimiter = ',')]
    models: Vec<String>,

    /// Force re-download even if the file is already verified
    #[arg(long)]
    force: bool,

    /// Dry-run: compute what would change without writing files
    #[arg(long)]
    dry_run: bool,

    /// Output JSON format
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct DoctorArgs {
    /// Output JSON format
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct EvalArgs {
    /// Project directory to evaluate (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Path to eval dataset JSON
    #[arg(long)]
    dataset: PathBuf,

    /// Top-K limit for evaluation (default: 10)
    #[arg(long, default_value_t = 10)]
    limit: usize,

    /// Profile names to evaluate (comma-separated). If omitted, uses the active profile.
    #[arg(long, value_delimiter = ',')]
    profiles: Vec<String>,

    /// Restrict evaluation to these model ids (comma-separated). If omitted, uses the profile roster.
    #[arg(long, value_delimiter = ',')]
    models: Vec<String>,

    /// Cache mode: warm (reuse process caches) vs cold (recreate search engine per case)
    #[arg(long, value_enum, default_value_t = EvalCacheModeFlag::Warm)]
    cache_mode: EvalCacheModeFlag,

    /// Write raw EvalOutput JSON artifact to this path
    #[arg(long)]
    out_json: Option<PathBuf>,

    /// Write a concise Markdown report to this path
    #[arg(long)]
    out_md: Option<PathBuf>,

    /// Output JSON format
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct EvalCompareArgs {
    /// Project directory to evaluate (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Path to eval dataset JSON
    #[arg(long)]
    dataset: PathBuf,

    /// Top-K limit for evaluation (default: 10)
    #[arg(long, default_value_t = 10)]
    limit: usize,

    /// Profile name for side A
    #[arg(long)]
    a_profile: String,

    /// Profile name for side B
    #[arg(long)]
    b_profile: String,

    /// Restrict side A to these model ids (comma-separated)
    #[arg(long, value_delimiter = ',')]
    a_models: Vec<String>,

    /// Restrict side B to these model ids (comma-separated)
    #[arg(long, value_delimiter = ',')]
    b_models: Vec<String>,

    /// Cache mode: warm (reuse process caches) vs cold (recreate search engine per case)
    #[arg(long, value_enum, default_value_t = EvalCacheModeFlag::Warm)]
    cache_mode: EvalCacheModeFlag,

    /// Write raw EvalCompareOutput JSON artifact to this path
    #[arg(long)]
    out_json: Option<PathBuf>,

    /// Write a concise Markdown report to this path
    #[arg(long)]
    out_md: Option<PathBuf>,

    /// Output JSON format
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ListSymbolsArgs {
    /// Project directory (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Filter by file path pattern
    #[arg(long)]
    file: Option<String>,

    /// Filter by symbol type (function, struct, enum, trait, impl)
    #[arg(long)]
    symbol_type: Option<String>,

    /// Output JSON format
    #[arg(long)]
    json: bool,
}

#[derive(Copy, Clone, ValueEnum)]
enum EmbedMode {
    Fast,
    Stub,
}

#[derive(Copy, Clone, ValueEnum)]
enum EvalCacheModeFlag {
    Warm,
    Cold,
}

impl EvalCacheModeFlag {
    const fn as_domain(self) -> EvalCacheMode {
        match self {
            EvalCacheModeFlag::Warm => EvalCacheMode::Warm,
            EvalCacheModeFlag::Cold => EvalCacheMode::Cold,
        }
    }
}

impl EmbedMode {
    const fn as_str(self) -> &'static str {
        match self {
            EmbedMode::Fast => "fast",
            EmbedMode::Stub => "stub",
        }
    }
}

fn parse_cache_backend(value: &str) -> Result<CacheBackend> {
    match value.to_lowercase().as_str() {
        "file" => Ok(CacheBackend::File),
        "memory" => Ok(CacheBackend::Memory),
        other => anyhow::bail!("Unsupported cache backend: {other}"),
    }
}

fn env_truthy(var: &str) -> bool {
    env::var(var)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn env_falsey(var: &str) -> bool {
    env::var(var)
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

fn cuda_disabled_by_env() -> bool {
    env_truthy("ORT_DISABLE_CUDA") || env_falsey("ORT_USE_CUDA")
}

fn embed_mode_is_stub() -> bool {
    env::var("CONTEXT_FINDER_EMBEDDING_MODE")
        .map(|v| v.eq_ignore_ascii_case("stub"))
        .unwrap_or(false)
}

fn command_action_requires_embeddings(action: &CommandAction) -> bool {
    matches!(
        action,
        CommandAction::Search
            | CommandAction::SearchWithContext
            | CommandAction::ContextPack
            | CommandAction::Index
            | CommandAction::GetContext
            | CommandAction::CompareSearch
            | CommandAction::Eval
            | CommandAction::EvalCompare
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = Cli::parse();

    if let Some(model) = &cli.embed_model {
        env::set_var("CONTEXT_FINDER_EMBEDDING_MODEL", model);
    }
    if let Some(dir) = &cli.model_dir {
        env::set_var("CONTEXT_FINDER_MODEL_DIR", dir);
    }
    if let Some(device) = cli.cuda_device {
        env::set_var("CONTEXT_FINDER_CUDA_DEVICE", device.to_string());
    }
    if let Some(limit_mb) = cli.cuda_mem_limit_mb {
        env::set_var("CONTEXT_FINDER_CUDA_MEM_LIMIT_MB", limit_mb.to_string());
    }
    if let Some(mode) = cli.embed_mode {
        env::set_var("CONTEXT_FINDER_EMBEDDING_MODE", mode.as_str());
    }

    let profile = cli
        .profile
        .clone()
        .or_else(|| env::var("CONTEXT_FINDER_PROFILE").ok())
        .unwrap_or_else(|| "quality".to_string());
    env::set_var("CONTEXT_FINDER_PROFILE", &profile);

    let needs_ort_bootstrap = match &cli.command {
        Commands::InstallModels(_) => false,
        Commands::Command(_) => false, // defer until we know the requested action
        _ => true,
    };
    if needs_ort_bootstrap && !embed_mode_is_stub() && !cuda_disabled_by_env() {
        let allow_cpu = env_truthy("CONTEXT_FINDER_ALLOW_CPU");
        if let Err(err) = bootstrap_gpu_env() {
            if matches!(cli.command, Commands::Doctor(_)) || allow_cpu {
                // Best-effort: allow `doctor` to report GPU/runtime issues and allow CPU fallback
                // flows to proceed without requiring CUDA runtime libs.
            } else {
                return Err(err).context("Failed to configure CUDA runtime paths");
            }
        }
    }

    // Auto-enable quiet mode when --json is used (to keep stdout clean for JSON parsing)
    // Also propagate explicit --quiet flag from subcommands
    let json_output = match &cli.command {
        Commands::Command(cmd) => {
            if cmd.quiet {
                cli.quiet = true;
            }
            true // command subcommand always outputs JSON
        }
        Commands::Index(args) => args.json,
        Commands::Search(args) => args.json,
        Commands::GetContext(args) => args.json,
        Commands::ListSymbols(args) => args.json,
        Commands::Map(args) => args.json,
        Commands::Context(args) => args.json,
        Commands::ContextPack(args) => args.json,
        Commands::InstallModels(args) => args.json,
        Commands::Doctor(args) => args.json,
        Commands::Eval(args) => args.json,
        Commands::EvalCompare(args) => args.json,
        _ => false,
    };
    if json_output {
        cli.quiet = true;
    }

    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if cli.quiet {
        builder.filter_level(log::LevelFilter::Warn);
    } else if cli.verbose {
        builder.filter_level(log::LevelFilter::Debug);
    }
    // Always silence ort crate unless verbose mode (ORT is extremely noisy)
    if !cli.verbose {
        builder.filter_module("ort", log::LevelFilter::Off);
    }
    builder.target(env_logger::Target::Stderr).init();

    let cache_cfg = CacheConfig {
        dir: PathBuf::from(&cli.cache_dir),
        ttl: Duration::from_secs(cli.cache_ttl_seconds),
        backend: parse_cache_backend(&cli.cache_backend)?,
        capacity: 32,
    };

    match cli.command {
        Commands::Command(args) => run_command(args, cache_cfg).await?,
        Commands::Index(args) => run_index(args, cache_cfg).await?,
        Commands::Search(args) => run_search(args, cache_cfg).await?,
        Commands::GetContext(args) => run_get_context(args, cache_cfg).await?,
        Commands::ListSymbols(args) => run_list_symbols(args, cache_cfg).await?,
        Commands::DaemonLoop(args) => heartbeat::run_daemon(args.socket).await?,
        Commands::ServeHttp(args) => serve_http(args, cache_cfg).await?,
        Commands::ServeGrpc(args) => serve_grpc(args, cache_cfg).await?,
        Commands::Map(args) => run_map(args, cache_cfg).await?,
        Commands::Context(args) => run_context(args, cache_cfg).await?,
        Commands::ContextPack(args) => run_context_pack(args, cache_cfg).await?,
        Commands::InstallModels(args) => run_install_models(args).await?,
        Commands::Doctor(args) => run_doctor(args).await?,
        Commands::Eval(args) => run_eval(args, cache_cfg).await?,
        Commands::EvalCompare(args) => run_eval_compare(args, cache_cfg).await?,
    }

    Ok(())
}

async fn run_eval(args: EvalArgs, cache_cfg: CacheConfig) -> Result<()> {
    let root = args.path.canonicalize().context("Invalid project path")?;
    let root_for_report = root.clone();
    let dataset = if args.dataset.is_relative() {
        root.join(&args.dataset)
    } else {
        args.dataset.clone()
    };

    let payload = EvalPayload {
        path: Some(root),
        dataset,
        limit: Some(args.limit),
        profiles: args.profiles.clone(),
        models: args.models.clone(),
        cache_mode: Some(args.cache_mode.as_domain()),
    };
    let request = CommandRequest {
        action: CommandAction::Eval,
        payload: serde_json::to_value(payload)?,
        config: None,
    };

    let response = command::execute(request, cache_cfg).await?;

    let eval_out = if response.is_error() {
        None
    } else {
        Some(
            serde_json::from_value::<EvalOutput>(response.data.clone())
                .context("Invalid eval output")?,
        )
    };

    if let Some(out) = &eval_out {
        if let Some(path) = &args.out_json {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, serde_json::to_string_pretty(out)?)?;
        }
        if let Some(path) = &args.out_md {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, report::render_eval_report(&root_for_report, out)?)?;
        }
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else if response.is_error() {
        eprintln!(
            "Error: {}",
            response.message.as_deref().unwrap_or("Unknown error")
        );
        std::process::exit(1);
    } else if let Some(out) = eval_out {
        for run in &out.runs {
            eprintln!(
                "profile={} models={} mean_mrr={:.3} mean_recall={:.3} p95_ms={} mean_bytes={:.1}",
                run.profile,
                run.models.join(","),
                run.summary.mean_mrr,
                run.summary.mean_recall,
                run.summary.p95_latency_ms,
                run.summary.mean_bytes
            );
        }
    }

    Ok(())
}

async fn run_eval_compare(args: EvalCompareArgs, cache_cfg: CacheConfig) -> Result<()> {
    let root = args.path.canonicalize().context("Invalid project path")?;
    let root_for_report = root.clone();
    let dataset = if args.dataset.is_relative() {
        root.join(&args.dataset)
    } else {
        args.dataset.clone()
    };

    let payload = EvalComparePayload {
        path: Some(root),
        dataset,
        limit: Some(args.limit),
        a: command::EvalCompareConfig {
            profile: args.a_profile.clone(),
            models: args.a_models.clone(),
        },
        b: command::EvalCompareConfig {
            profile: args.b_profile.clone(),
            models: args.b_models.clone(),
        },
        cache_mode: Some(args.cache_mode.as_domain()),
    };
    let request = CommandRequest {
        action: CommandAction::EvalCompare,
        payload: serde_json::to_value(payload)?,
        config: None,
    };

    let response = command::execute(request, cache_cfg).await?;

    let compare_out = if response.is_error() {
        None
    } else {
        Some(
            serde_json::from_value::<EvalCompareOutput>(response.data.clone())
                .context("Invalid eval_compare output")?,
        )
    };

    if let Some(out) = &compare_out {
        if let Some(path) = &args.out_json {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, serde_json::to_string_pretty(out)?)?;
        }
        if let Some(path) = &args.out_md {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(
                path,
                report::render_eval_compare_report(&root_for_report, out)?,
            )?;
        }
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else if response.is_error() {
        eprintln!(
            "Error: {}",
            response.message.as_deref().unwrap_or("Unknown error")
        );
        std::process::exit(1);
    } else if let Some(out) = compare_out {
        eprintln!(
            "A={} B={} Δmrr={:.3} Δrecall={:.3} Δp95_ms={} (wins: A={} B={} ties={})",
            out.a.profile,
            out.b.profile,
            out.summary.delta_mean_mrr,
            out.summary.delta_mean_recall,
            out.summary.delta_p95_latency_ms,
            out.summary.a_wins,
            out.summary.b_wins,
            out.summary.ties
        );
    }

    Ok(())
}

async fn run_command(args: CommandArgs, cache_cfg: CacheConfig) -> Result<()> {
    let raw = read_payload(&args)?;
    let request: CommandRequest =
        serde_json::from_str(&raw).context("Invalid JSON passed to --json/--file")?;

    if command_action_requires_embeddings(&request.action)
        && !embed_mode_is_stub()
        && !cuda_disabled_by_env()
    {
        let allow_cpu = env_truthy("CONTEXT_FINDER_ALLOW_CPU");
        if let Err(err) = bootstrap_gpu_env() {
            if !allow_cpu {
                return Err(err).context("Failed to configure CUDA runtime paths");
            }
        }
    }

    let response = match command::execute(request, cache_cfg).await {
        Ok(resp) => resp,
        Err(err) => {
            let message = format!("{err:#}");
            let hints = classify_error(&message);
            CommandResponse::error_with_hints(message, hints)
        }
    };

    let output = if args.pretty {
        serde_json::to_string_pretty(&response)?
    } else {
        serde_json::to_string(&response)?
    };
    println!("{output}");

    if response.is_error() {
        std::process::exit(1);
    }
    Ok(())
}

fn read_payload(args: &CommandArgs) -> Result<String> {
    if let Some(raw) = &args.json {
        return Ok(raw.clone());
    }
    if let Some(path) = &args.file {
        return fs::read_to_string(path)
            .with_context(|| format!("Failed to read JSON from {}", path.display()));
    }

    let mut buffer = String::new();
    io::stdin()
        .read_to_string(&mut buffer)
        .context("Failed to read JSON from stdin")?;

    if buffer.trim().is_empty() {
        anyhow::bail!("Command request is empty. Provide --json, --file, or pipe JSON via stdin.");
    }

    Ok(buffer)
}

/// Index a project directory
async fn run_index(args: IndexArgs, cache_cfg: CacheConfig) -> Result<()> {
    let path = args.path.canonicalize().context("Invalid project path")?;
    let payload = IndexPayload {
        path: Some(path.clone()),
        full: args.force,
        models: args.models.clone(),
        experts: args.experts,
    };
    let request = CommandRequest {
        action: CommandAction::Index,
        payload: serde_json::to_value(payload)?,
        config: None,
    };

    let response = command::execute(request, cache_cfg).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else if response.is_error() {
        eprintln!(
            "Error: {}",
            response.message.as_deref().unwrap_or("Unknown error")
        );
        std::process::exit(1);
    } else if let Ok(index_resp) = serde_json::from_value::<IndexResponse>(response.data) {
        eprintln!(
            "Indexed {} files, {} chunks in {}ms",
            index_resp.stats.files, index_resp.stats.chunks, index_resp.stats.time_ms
        );
    }
    Ok(())
}

/// Search for code matching a query
async fn run_search(args: SearchArgs, cache_cfg: CacheConfig) -> Result<()> {
    let path = args.path.canonicalize().context("Invalid project path")?;
    let payload = SearchPayload {
        query: args.query.clone(),
        limit: Some(args.limit),
        project: Some(path.clone()),
        trace: None,
    };
    let request = CommandRequest {
        action: CommandAction::Search,
        payload: serde_json::to_value(payload)?,
        config: None,
    };

    let response = command::execute(request, cache_cfg).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else if response.is_error() {
        eprintln!(
            "Error: {}",
            response.message.as_deref().unwrap_or("Unknown error")
        );
        std::process::exit(1);
    } else if let Ok(search_out) = serde_json::from_value::<SearchOutput>(response.data) {
        for (i, result) in search_out.results.iter().enumerate() {
            println!("{}. {} (score: {:.3})", i + 1, result.file, result.score);
            if let Some(symbol) = &result.symbol {
                println!("   Symbol: {}", symbol);
            }
            println!("   Lines: {}-{}", result.start_line, result.end_line);
            println!();
        }
    }
    Ok(())
}

/// Get context for code understanding
async fn run_get_context(args: GetContextArgs, cache_cfg: CacheConfig) -> Result<()> {
    let path = args.path.canonicalize().context("Invalid project path")?;
    if args.queries.is_empty() {
        anyhow::bail!("At least one query is required");
    }

    // Run search for each query and aggregate results
    let mut all_results = Vec::new();
    for query in &args.queries {
        let payload = SearchPayload {
            query: query.clone(),
            limit: Some(args.limit),
            project: Some(path.clone()),
            trace: None,
        };
        let request = CommandRequest {
            action: CommandAction::Search,
            payload: serde_json::to_value(payload)?,
            config: None,
        };

        let response = command::execute(request, cache_cfg.clone()).await?;
        if response.is_error() {
            eprintln!(
                "Error: {}",
                response.message.as_deref().unwrap_or("Unknown error")
            );
            std::process::exit(1);
        }
        if let Ok(search_out) = serde_json::from_value::<SearchOutput>(response.data) {
            all_results.extend(search_out.results);
        }
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&all_results)?);
    } else {
        eprintln!(
            "Found {} results for {} queries",
            all_results.len(),
            args.queries.len()
        );
        eprintln!();
        for (i, result) in all_results.iter().enumerate() {
            let symbol_info = match (&result.symbol, &result.chunk_type) {
                (Some(sym), Some(kind)) => format!(" [{} {}]", kind, sym),
                (Some(sym), None) => format!(" [{}]", sym),
                _ => String::new(),
            };
            println!(
                "# {} {} lines {}-{} (score: {:.3}){}",
                i + 1,
                result.file,
                result.start_line,
                result.end_line,
                result.score,
                symbol_info
            );
            println!("{}", result.content);
            println!();
        }
    }
    Ok(())
}

/// List symbols in the indexed project
async fn run_list_symbols(args: ListSymbolsArgs, cache_cfg: CacheConfig) -> Result<()> {
    let path = args.path.canonicalize().context("Invalid project path")?;
    let file = args.file.clone().unwrap_or_else(|| "*".to_string());
    let payload = ListSymbolsPayload {
        file,
        project: Some(path.clone()),
    };
    let request = CommandRequest {
        action: CommandAction::ListSymbols,
        payload: serde_json::to_value(payload)?,
        config: None,
    };

    let response = command::execute(request, cache_cfg).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else if response.is_error() {
        eprintln!(
            "Error: {}",
            response.message.as_deref().unwrap_or("Unknown error")
        );
        std::process::exit(1);
    } else if let Ok(symbols_out) = serde_json::from_value::<SymbolsOutput>(response.data) {
        let type_filter = args.symbol_type.as_deref();
        let multi_file = symbols_out.files_count.is_some();

        if multi_file {
            // Multi-file mode: show file path per symbol
            if let Some(count) = symbols_out.files_count {
                eprintln!(
                    "Found {} symbols across {} files:",
                    symbols_out.symbols.len(),
                    count
                );
            }
        }

        for symbol in &symbols_out.symbols {
            if type_filter.is_none_or(|t| symbol.symbol_type.eq_ignore_ascii_case(t)) {
                let file_display = symbol.file.as_deref().unwrap_or(&symbols_out.file);
                println!(
                    "{} {} ({}:{})",
                    symbol.symbol_type, symbol.name, file_display, symbol.line
                );
            }
        }
    }
    Ok(())
}

/// Show project structure overview
async fn run_map(args: MapArgs, cache_cfg: CacheConfig) -> Result<()> {
    let path = args.path.canonicalize().context("Invalid project path")?;
    let payload = MapPayload {
        project: Some(path.clone()),
        depth: args.depth,
        limit: args.limit,
    };
    let request = CommandRequest {
        action: CommandAction::Map,
        payload: serde_json::to_value(payload)?,
        config: None,
    };

    let response = command::execute(request, cache_cfg).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else if response.is_error() {
        eprintln!(
            "Error: {}",
            response.message.as_deref().unwrap_or("Unknown error")
        );
        std::process::exit(1);
    } else if let Ok(map_out) = serde_json::from_value::<MapOutput>(response.data) {
        eprintln!(
            "Project: {} files, {} chunks, {} lines",
            map_out.total_files,
            map_out.total_chunks,
            map_out.total_lines.unwrap_or(0)
        );
        eprintln!();

        for node in &map_out.nodes {
            let coverage = node
                .coverage_lines_pct
                .map(|p| format!("{:.1}%", p))
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{:<40} {:>4} files {:>5} chunks ({} of code)",
                node.path, node.files, node.chunks, coverage
            );

            if let Some(symbols) = &node.top_symbols {
                for sym in symbols.iter().take(3) {
                    let parent = sym
                        .parent
                        .as_deref()
                        .map(|p| format!(" in {}", p))
                        .unwrap_or_default();
                    println!("  - {} {}{}", sym.symbol_type, sym.name, parent);
                }
            }
        }
    }
    Ok(())
}

/// Search with automatic graph context (best for AI agents)
async fn run_context(args: ContextArgs, cache_cfg: CacheConfig) -> Result<()> {
    let path = args.path.canonicalize().context("Invalid project path")?;
    let strategy = args.strategy.as_deref().and_then(SearchStrategy::from_name);
    let payload = SearchWithContextPayload {
        query: args.query.clone(),
        limit: Some(args.limit),
        project: Some(path.clone()),
        strategy,
        show_graph: Some(args.show_graph),
        trace: None,
        language: args.language.clone(),
        reuse_graph: Some(true),
    };
    let request = CommandRequest {
        action: CommandAction::SearchWithContext,
        payload: serde_json::to_value(payload)?,
        config: None,
    };

    let response = command::execute(request, cache_cfg).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else if response.is_error() {
        eprintln!(
            "Error: {}",
            response.message.as_deref().unwrap_or("Unknown error")
        );
        std::process::exit(1);
    } else if let Ok(search_out) = serde_json::from_value::<SearchOutput>(response.data) {
        eprintln!(
            "Found {} results for query '{}'",
            search_out.results.len(),
            args.query
        );
        eprintln!();

        for (i, result) in search_out.results.iter().enumerate() {
            let symbol_info = match (&result.symbol, &result.chunk_type) {
                (Some(sym), Some(kind)) => format!(" [{} {}]", kind, sym),
                (Some(sym), None) => format!(" [{}]", sym),
                _ => String::new(),
            };
            println!(
                "# {} {} lines {}-{} (score: {:.3}){}",
                i + 1,
                result.file,
                result.start_line,
                result.end_line,
                result.score,
                symbol_info
            );
            println!("{}", result.content);

            // Show related code from graph
            if let Some(related) = &result.related {
                if !related.is_empty() {
                    println!();
                    println!("  Related ({}):", related.len());
                    for rel in related.iter().take(5) {
                        let rel_sym = rel
                            .symbol
                            .as_deref()
                            .map(|s| format!(" [{}]", s))
                            .unwrap_or_default();
                        println!(
                            "    - {} lines {}-{}{} ({})",
                            rel.file,
                            rel.start_line,
                            rel.end_line,
                            rel_sym,
                            rel.relationship.join(" -> ")
                        );
                    }
                }
            }
            println!();
        }
    }
    Ok(())
}

async fn run_context_pack(args: ContextPackArgs, cache_cfg: CacheConfig) -> Result<()> {
    let path = args.path.canonicalize().context("Invalid project path")?;
    let strategy = args.strategy.as_deref().and_then(SearchStrategy::from_name);
    let payload = ContextPackPayload {
        query: args.query.clone(),
        limit: Some(args.limit),
        project: Some(path.clone()),
        strategy,
        max_chars: args.max_chars,
        max_related_per_primary: args.max_related_per_primary,
        trace: if args.trace { Some(true) } else { None },
        language: args.language.clone(),
        reuse_graph: Some(true),
    };
    let request = CommandRequest {
        action: CommandAction::ContextPack,
        payload: serde_json::to_value(payload)?,
        config: None,
    };

    let response = command::execute(request, cache_cfg).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else if response.is_error() {
        eprintln!(
            "Error: {}",
            response.message.as_deref().unwrap_or("Unknown error")
        );
        std::process::exit(1);
    } else if let Ok(pack) = serde_json::from_value::<ContextPackOutput>(response.data) {
        eprintln!(
            "Packed {} items ({} / {} chars)",
            pack.items.len(),
            pack.budget.used_chars,
            pack.budget.max_chars
        );
        eprintln!("Model: {}, profile: {}", pack.model_id, pack.profile);
    }

    Ok(())
}

async fn run_install_models(args: InstallModelsArgs) -> Result<()> {
    let model_dir = models::resolve_model_dir();
    let report = models::install_models(&model_dir, &args.models, args.force, args.dry_run).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        eprintln!("Model dir: {}", report.model_dir);
        if !report.selected_models.is_empty() {
            eprintln!("Selected models: {}", report.selected_models.join(", "));
        }
        if !report.skipped.is_empty() {
            eprintln!("Skipped: {}", report.skipped.len());
        }
        if !report.downloaded.is_empty() {
            eprintln!("Downloaded: {}", report.downloaded.len());
        }
    }

    Ok(())
}

async fn run_doctor(args: DoctorArgs) -> Result<()> {
    let model_dir = models::resolve_model_dir();
    let report = models::doctor(&model_dir);

    let ok = report.manifest_ok
        && report.models.iter().all(|m| m.ok)
        && (report.gpu_ok || report.gpu_error.is_none());

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        eprintln!("Model dir: {}", report.model_dir);
        eprintln!("Profile: {}", report.profile);
        eprintln!(
            "Embedding mode/model: {} / {}",
            report.embedding_mode, report.embedding_model
        );
        eprintln!("Allow CPU fallback: {}", report.allow_cpu_fallback);
        if report.manifest_ok {
            eprintln!("Manifest: ok");
        } else {
            eprintln!(
                "Manifest: error ({})",
                report.manifest_error.as_deref().unwrap_or("unknown")
            );
        }
        if report.gpu_ok {
            eprintln!("GPU/runtime: ok");
        } else if let Some(err) = &report.gpu_error {
            eprintln!("GPU/runtime: error ({err})");
        }

        for model in &report.models {
            if model.ok {
                continue;
            }
            eprintln!();
            eprintln!("Model '{}' issues:", model.id);
            for miss in &model.missing_assets {
                eprintln!("  - missing: {miss}");
            }
            for bad in &model.bad_sha256 {
                eprintln!(
                    "  - sha256 mismatch: {} (expected {}, got {})",
                    bad.path, bad.expected, bad.actual
                );
            }
        }
    }

    if !ok {
        std::process::exit(1);
    }

    Ok(())
}

async fn serve_http(args: ServeArgs, cache_cfg: CacheConfig) -> Result<()> {
    let state = std::sync::Arc::new(HttpState {
        cache: cache_cfg,
        health: HealthPort,
    });
    let app = Router::new()
        .route(
            "/command",
            post({
                let state = state.clone();
                move |body| http_handler(body, state.clone())
            }),
        )
        .route(
            "/health",
            get({
                let state = state.clone();
                move || http_health(state.clone())
            }),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    println!("Serving Command API on http://{}/command", args.bind);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn serve_grpc(args: ServeGrpcArgs, cache_cfg: CacheConfig) -> Result<()> {
    let addr = args.bind.parse()?;
    let server = grpc::GrpcServer::new(cache_cfg);
    println!("Serving gRPC Command API on {addr}");
    Server::builder()
        .add_service(server.into_server())
        .serve(addr)
        .await?;
    Ok(())
}

async fn http_handler(
    body: axum::body::Bytes,
    state: std::sync::Arc<HttpState>,
) -> Result<Response, StatusCode> {
    let request: CommandRequest =
        serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let response = match command::execute(request, state.cache.clone()).await {
        Ok(resp) => resp,
        Err(err) => {
            let message = format!("{err:#}");
            let hints = classify_error(&message);
            CommandResponse::error_with_hints(message, hints)
        }
    };

    let bytes = serde_json::to_vec(&response).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(HttpResponse::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(bytes))
        .expect("valid HTTP response"))
}

async fn http_health(state: std::sync::Arc<HttpState>) -> Result<Response, StatusCode> {
    let root = std::env::current_dir().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let report = state
        .health
        .probe(&root)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let bytes = serde_json::to_vec(&report).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(HttpResponse::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(bytes))
        .expect("valid HTTP response"))
}

fn bootstrap_gpu_env() -> Result<()> {
    let ort_lib =
        resolve_ort_lib().context("ONNX Runtime CUDA libraries not found in default locations")?;
    env::set_var("ORT_LIB_LOCATION", &ort_lib);

    let mut paths = vec![ort_lib];
    paths.extend(nvidia_cuda_libs());
    prepend_ld_library_path(paths);
    log::debug!(
        "ORT_LIB_LOCATION={} LD_LIBRARY_PATH={}",
        env::var("ORT_LIB_LOCATION").unwrap_or_default(),
        env::var("LD_LIBRARY_PATH").unwrap_or_default()
    );
    Ok(())
}

fn resolve_ort_lib() -> Option<PathBuf> {
    if let Ok(path) = env::var("ORT_LIB_LOCATION") {
        let candidate = PathBuf::from(path);
        if has_cuda_provider(&candidate) {
            return Some(candidate);
        }
    }

    let home = env::var("HOME").ok()?;
    let root = Path::new(&home)
        .join(".cache")
        .join("ort.pyke.io")
        .join("dfbin")
        .join("x86_64-unknown-linux-gnu");

    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("onnxruntime").join("lib");
        if has_cuda_provider(&candidate) {
            return Some(candidate);
        }
    }

    None
}

fn has_cuda_provider(dir: &Path) -> bool {
    dir.join("libonnxruntime_providers_cuda.so").exists()
}

fn nvidia_cuda_libs() -> Vec<PathBuf> {
    let mut libs = Vec::new();
    let home = env::var("HOME").unwrap_or_default();
    let base = Path::new(&home)
        .join(".local")
        .join("lib")
        .join("python3.12")
        .join("site-packages")
        .join("nvidia");
    let names = ["cublas", "cuda_runtime", "curand", "cufft", "cudnn"];
    for name in names {
        let dir = base.join(name).join("lib");
        if dir.exists() {
            libs.push(dir);
        }
    }
    let system_cuda = [
        Path::new("/usr/local/cuda/lib64"),
        Path::new("/usr/local/cuda/targets/x86_64-linux/lib"),
    ];
    for dir in system_cuda {
        if dir.exists() {
            libs.push(dir.to_path_buf());
        }
    }
    libs
}

fn prepend_ld_library_path(paths: Vec<PathBuf>) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut ordered: Vec<String> = Vec::new();

    for path in paths {
        if !path.exists() {
            continue;
        }
        let value = path.to_string_lossy().into_owned();
        if seen.insert(value.clone()) {
            ordered.push(value);
        }
    }

    if let Ok(existing) = env::var("LD_LIBRARY_PATH") {
        for part in existing.split(':').filter(|p| !p.is_empty()) {
            if seen.insert(part.to_string()) {
                ordered.push(part.to_string());
            }
        }
    }

    if !ordered.is_empty() {
        env::set_var("LD_LIBRARY_PATH", ordered.join(":"));
    }
}

#[derive(Clone)]
struct HttpState {
    cache: CacheConfig,
    health: HealthPort,
}
