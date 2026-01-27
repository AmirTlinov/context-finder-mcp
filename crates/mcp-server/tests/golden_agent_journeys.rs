use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

mod support;

fn next_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.trim_start().starts_with("N: next:"))
        .collect()
}

async fn start_service(cwd: &Path) -> Result<RunningService<RoleClient, ()>> {
    let bin = support::locate_context_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.current_dir(cwd);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");
    cmd.env("RUST_LOG", "warn");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")?;
    Ok(service)
}

async fn call_tool_text(
    service: &RunningService<RoleClient, ()>,
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
    Ok(result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or("")
        .to_string())
}

#[tokio::test]
async fn golden_rg_zero_hit_suggests_text_search() -> Result<()> {
    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").context("write main.rs")?;

    let service = start_service(root).await?;
    let text = call_tool_text(
        &service,
        "rg",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "no_such_token_12345",
            "max_chars": 2000,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert!(
        text.contains("Matches: 0"),
        "expected zero matches summary, got:\n{text}"
    );
    let next = next_lines(&text);
    assert_eq!(
        next.len(),
        1,
        "expected exactly one next step hint, got:\n{text}"
    );
    assert!(
        next[0].contains("next: text_search"),
        "expected next step hint for text_search, got:\n{text}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn golden_text_search_zero_hit_suggests_rg() -> Result<()> {
    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").context("write main.rs")?;

    let service = start_service(root).await?;
    let text = call_tool_text(
        &service,
        "text_search",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "no_such_token_12345",
            "max_chars": 2000,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert!(
        text.contains("Matches: 0"),
        "expected zero matches summary, got:\n{text}"
    );
    let next = next_lines(&text);
    assert_eq!(
        next.len(),
        1,
        "expected exactly one next step hint, got:\n{text}"
    );
    assert!(
        next[0].contains("next: rg"),
        "expected next step hint for rg, got:\n{text}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn golden_tree_zero_hit_suggests_ls() -> Result<()> {
    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    // Empty repo: tree has nothing to aggregate, but should still guide the agent.
    let service = start_service(root).await?;
    let text = call_tool_text(
        &service,
        "tree",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "depth": 2,
            "limit": 20,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert!(
        text.contains("tree: 0 directories"),
        "expected empty tree output, got:\n{text}"
    );
    let next = next_lines(&text);
    assert_eq!(
        next.len(),
        1,
        "expected exactly one next step hint, got:\n{text}"
    );
    assert!(
        next[0].contains("next: ls"),
        "expected next step hint for ls, got:\n{text}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
