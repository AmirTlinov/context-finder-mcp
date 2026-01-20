//! Context MCP Server
//!
//! Provides semantic code search capabilities to AI agents via MCP protocol.
//!
//! ## Tools
//!
//! - `repo_onboarding_pack` - Map + key docs + next_actions (best first call)
//! - `read_pack` - One-call file/grep/query/onboarding with cursor continuation
//! - `context_pack` - Bounded semantic pack (primary + related halo)
//! - `batch` - Multiple tool calls under one max_chars budget with $ref
//! - `help` - `.context` legend (A/R/N/M) and usage notes
//! - `cat` - Bounded file window (root-locked)
//! - `rg` - Regex matches with before/after context hunks
//! - `grep` - Alias for `rg`
//! - `file_slice` - Legacy name for `cat`
//! - `grep_context` - Legacy name for `rg`
//! - `ls` - Bounded file enumeration (glob/substring filter)
//! - `find` - Alias for `ls`
//! - `list_files` - Legacy name for `ls`
//! - `text_search` - Bounded text search (corpus or FS fallback)
//! - `search` - Semantic search using natural language
//! - `context` - Search with automatic graph-based context (calls, dependencies)
//! - `impact` - Find symbol usages and transitive impact
//! - `trace` - Call chain between two symbols
//! - `explain` - Symbol details, deps, dependents, docs
//! - `overview` - Architecture snapshot (layers, entry points)
//! - `tree` - Project structure overview (directories, files, top symbols)
//! - `map` - Legacy name for `tree`
//! - `doctor` - Diagnose model/GPU/index configuration
//! - `notebook_pack` - Agent notebook: list durable anchors + runbooks (cross-session)
//! - `notebook_edit` - Agent notebook: upsert/delete anchors + runbooks (explicit writes)
//! - `notebook_suggest` - Notebook autopilot: suggest evidence-backed anchors + runbooks (read-only)
//! - `runbook_pack` - Runbook runner: TOC + expand sections (cursor-based)
//!
//! ## Usage
//!
//! Add to your MCP client configuration:
//! ```json
//! {
//!   "mcpServers": {
//!     "context": {
//!       "command": "context-mcp"
//!     }
//!   }
//! }
//! ```

use anyhow::Result;
use rmcp::ServiceExt;
use std::env;

mod index_warmup;
mod mcp_daemon;
mod runtime_env;
mod stdio_hybrid;
#[cfg(test)]
mod test_support;
mod tools;

use stdio_hybrid::stdio_hybrid_server_agent_oneshot_safe;
use tools::catalog;
use tools::ContextFinderService;

fn print_help() {
    println!("Context MCP server");
    println!();
    println!("Usage: context-mcp [--print-tools|--version|--help]");
    println!("       context-mcp daemon [--socket <path>]");
    println!();
    println!("Flags:");
    println!("  --print-tools  Print tool inventory as JSON and exit");
    println!("  --version      Print version and exit");
    println!("  --help         Print this help and exit");
    println!();
    println!("Env:");
    println!(
        "  CONTEXT_MCP_SHARED=0  Disable shared backend daemon (run in-process; mostly for tests)"
    );
    println!("  CONTEXT_MCP_SOCKET    Override daemon socket path");
}

enum CliAction {
    Exit(i32),
    RunDaemon { socket: std::path::PathBuf },
}

fn parse_socket_arg(args: &[String]) -> Option<std::path::PathBuf> {
    let mut it = args.iter().peekable();
    while let Some(arg) = it.next() {
        if arg == "--socket" {
            let next = it.next()?;
            let trimmed = next.trim();
            if trimmed.is_empty() {
                return None;
            }
            return Some(std::path::PathBuf::from(trimmed));
        }
    }
    None
}

fn handle_cli_args() -> Option<CliAction> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        return None;
    }

    if args.len() == 1 {
        match args[0].as_str() {
            "--stdio" | "stdio" => {
                // Compatibility: some MCP clients unconditionally pass `--stdio`.
                return None;
            }
            "--print-tools" => {
                let payload = catalog::tool_inventory_json(env!("CARGO_PKG_VERSION"));
                println!("{}", payload);
                return Some(CliAction::Exit(0));
            }
            "--version" | "-V" => {
                println!("context-mcp {}", env!("CARGO_PKG_VERSION"));
                return Some(CliAction::Exit(0));
            }
            "--help" | "-h" => {
                print_help();
                return Some(CliAction::Exit(0));
            }
            _ => {}
        }
    }

    if args[0] == "daemon" {
        let socket = parse_socket_arg(&args[1..]).unwrap_or_else(mcp_daemon::socket_path_from_env);
        return Some(CliAction::RunDaemon { socket });
    }

    // Be permissive: when launched under agent tooling, extra args can appear
    // (wrappers, transport selectors, etc). Starting the server is better than
    // failing the toolchain.
    if logging_enabled() {
        log::warn!("Ignoring unknown arguments: {}", args.join(" "));
    }
    None
}

fn logging_enabled() -> bool {
    // Protocol purity: any non-MCP bytes on stdout will break clients, and some MCP clients
    // may merge stderr into stdout. Default to silent unless explicitly enabled.
    std::env::var("CONTEXT_MCP_LOG")
        .or_else(|_| std::env::var("CONTEXT_FINDER_MCP_LOG"))
        .ok()
        .map(|v| {
            let v = v.trim();
            !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
        })
        .unwrap_or(false)
}

fn shared_backend_enabled() -> bool {
    match std::env::var("CONTEXT_MCP_SHARED")
        .or_else(|_| std::env::var("CONTEXT_FINDER_MCP_SHARED"))
    {
        Ok(value) => {
            let value = value.trim();
            !(value == "0" || value.eq_ignore_ascii_case("false"))
        }
        Err(_) => true, // default: shared backend (agent-native multi-session UX)
    }
}

fn background_bootstrap_enabled() -> bool {
    // In deterministic/stub modes (and some CI/test harnesses), skip best-effort bootstrapping.
    // Semantic paths will still bootstrap lazily when needed.
    let daemon_disabled = std::env::var("CONTEXT_DISABLE_DAEMON")
        .or_else(|_| std::env::var("CONTEXT_FINDER_DISABLE_DAEMON"))
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if daemon_disabled {
        return false;
    }

    let stub_embeddings = std::env::var("CONTEXT_EMBEDDING_MODE")
        .or_else(|_| std::env::var("CONTEXT_FINDER_EMBEDDING_MODE"))
        .ok()
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("stub"));
    if stub_embeddings {
        return false;
    }

    true
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(action) = handle_cli_args() {
        match action {
            CliAction::Exit(code) => std::process::exit(code),
            CliAction::RunDaemon { socket } => {
                mcp_daemon::run_daemon(&socket).await?;
                return Ok(());
            }
        }
    }

    if logging_enabled() {
        // Configure logging to stderr only (stdout is for MCP protocol). We still keep it
        // opt-in to avoid breaking clients that merge stderr into stdout.
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
            .target(env_logger::Target::Stderr)
            .filter_module("ort", log::LevelFilter::Off) // Silence ONNX Runtime
            .init();
    }

    if logging_enabled() {
        log::info!("Starting Context MCP server");
    }

    if shared_backend_enabled() {
        let socket = mcp_daemon::socket_path_from_env();
        match mcp_daemon::proxy_stdio_to_daemon(&socket).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                // Agent-native robustness: shared backend is the fast path, but it must never be
                // a single point of failure. If the daemon cannot start (stale socket, race, etc)
                // fall back to an in-process server instead of crashing the MCP session.
                if logging_enabled() {
                    log::warn!("Shared MCP backend unavailable; falling back to in-process server ({err:#})");
                }
            }
        }
    }

    // Best-effort bootstrap is useful (model dir / GPU libs) but must never delay MCP startup.
    // Run it in the background so the server stays responsive even on cold machines.
    let log_enabled = logging_enabled();
    if background_bootstrap_enabled() {
        tokio::task::spawn_blocking(move || {
            let report = runtime_env::bootstrap_best_effort();
            if log_enabled {
                for warning in report.warnings {
                    log::warn!("{warning}");
                }
            }
        });
    }

    let service = ContextFinderService::new();
    let server = service
        .serve(stdio_hybrid_server_agent_oneshot_safe())
        .await?;

    // Wait for shutdown
    server.waiting().await?;

    if logging_enabled() {
        log::info!("Context MCP server stopped");
    }
    Ok(())
}
