use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RunningService, Service, ServiceExt},
    transport::TokioChildProcess,
};
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;

fn locate_context_finder_mcp_bin() -> Result<PathBuf> {
    if let Some(path) = option_env!("CARGO_BIN_EXE_context-finder-mcp") {
        return Ok(PathBuf::from(path));
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(target_profile_dir) = exe.parent().and_then(|p| p.parent()) {
            let candidate = target_profile_dir.join("context-finder-mcp");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .ancestors()
        .nth(2)
        .context("failed to resolve repo root from CARGO_MANIFEST_DIR")?;
    for rel in [
        "target/debug/context-finder-mcp",
        "target/release/context-finder-mcp",
    ] {
        let candidate = repo_root.join(rel);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    anyhow::bail!("failed to locate context-finder-mcp binary")
}

async fn start_mcp_server(
) -> Result<RunningService<rmcp::RoleClient, impl Service<rmcp::RoleClient>>> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")
}

async fn call_tool_text(
    service: &RunningService<rmcp::RoleClient, impl Service<rmcp::RoleClient>>,
    name: &str,
    args: serde_json::Value,
) -> Result<String> {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: name.to_string().into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling tool")?
    .context("call tool")?;

    assert_ne!(result.is_error, Some(true), "{name} returned error");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("tool did not return text content")?;
    Ok(text.to_string())
}

#[tokio::test]
async fn help_is_the_only_tool_that_returns_legend() -> Result<()> {
    let service = start_mcp_server().await?;

    let help = call_tool_text(&service, "help", serde_json::json!({})).await?;
    assert!(
        help.starts_with("[LEGEND]\n") && help.contains("[CONTENT]\n"),
        "help should return a self-contained legend"
    );

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("lib.rs"),
        "pub fn alpha() {}\n\npub fn beta() { alpha(); }\n",
    )
    .context("write src/lib.rs")?;

    let text_search = call_tool_text(
        &service,
        "text_search",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "alpha",
            "max_chars": 900,
            "response_mode": "full",
        }),
    )
    .await?;
    assert!(
        text_search.starts_with("[CONTENT]\n"),
        "other tools should be low-noise even in full mode (no legend)"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn help_supports_tools_topic_and_lists_known_topics_on_unknown() -> Result<()> {
    let service = start_mcp_server().await?;

    let tools = call_tool_text(
        &service,
        "help",
        serde_json::json!({
            "topic": "tools"
        }),
    )
    .await?;
    assert!(
        tools.contains("Tool inventory:"),
        "expected tools topic header"
    );
    assert!(
        tools.contains("- read_pack:"),
        "expected read_pack to be listed in tools inventory"
    );

    let unknown = call_tool_text(
        &service,
        "help",
        serde_json::json!({
            "topic": "definitely-not-a-topic"
        }),
    )
    .await?;
    assert!(
        unknown.contains("available topics:"),
        "expected unknown topic to list available topics"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
