//! Context Finder MCP Server
//!
//! Provides semantic code search capabilities to AI agents via MCP protocol.
//!
//! ## Tools
//!
//! - `map` - Get project structure overview (directories, files, top symbols)
//! - `search` - Semantic code search using natural language
//! - `context` - Search with automatic graph-based context (calls, dependencies)
//! - `index` - Index a project directory for semantic search
//!
//! ## Usage
//!
//! Add to your MCP client configuration:
//! ```json
//! {
//!   "mcpServers": {
//!     "context-finder": {
//!       "command": "context-finder-mcp"
//!     }
//!   }
//! }
//! ```

use anyhow::Result;
use rmcp::transport::stdio;
use rmcp::ServiceExt;

mod tools;

use tools::ContextFinderService;

#[tokio::main]
async fn main() -> Result<()> {
    // Configure logging to stderr only (stdout is for MCP protocol)
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .target(env_logger::Target::Stderr)
        .filter_module("ort", log::LevelFilter::Off) // Silence ONNX Runtime
        .init();

    log::info!("Starting Context Finder MCP server");

    // Create and start the MCP server
    let service = ContextFinderService::new();
    let server = service.serve(stdio()).await?;

    // Wait for shutdown
    server.waiting().await?;

    log::info!("Context Finder MCP server stopped");
    Ok(())
}
