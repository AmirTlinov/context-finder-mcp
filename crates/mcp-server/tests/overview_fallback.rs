use anyhow::{Context, Result};
use context_vector_store::context_dir_for_project_root;
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use std::time::Duration;
use tokio::process::Command;

mod support;

async fn start_service() -> Result<(tempfile::TempDir, RunningService<RoleClient, ()>)> {
    let bin = support::locate_context_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tmp = tempfile::tempdir().context("tempdir")?;
    Ok((tmp, service))
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
    .context("timeout calling tool")??;

    anyhow::ensure!(
        result.is_error != Some(true),
        "tool {name} returned error: {result:?}"
    );
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .context("tool did not return text output")
}

#[tokio::test]
async fn overview_falls_back_to_filesystem_when_index_is_stale() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("main.rs"),
        "fn main() {\n  // TODO: wire up handler\n}\n",
    )
    .context("write src/main.rs")?;

    let context_dir = context_dir_for_project_root(root);
    // Create a stale semantic index marker: index.json exists but watermark.json is missing.
    // This should make meta.index_state.stale=true (WatermarkMissing) without requiring a real
    // index build.
    std::fs::create_dir_all(context_dir.join("indexes").join("bge-small"))
        .context("mkdir stale index dir")?;
    std::fs::write(
        context_dir
            .join("indexes")
            .join("bge-small")
            .join("index.json"),
        "{}\n",
    )
    .context("write fake index.json")?;

    let text = call_tool_text(
        &service,
        "overview",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "response_mode": "minimal",
        }),
    )
    .await?;

    assert!(text.contains("[CONTENT]"));
    assert!(text.contains("A: overview:"), "expected overview answer");
    assert!(text.contains("layers:"), "expected layers section");
    assert!(
        text.contains("- src (files=1"),
        "expected src layer to be surfaced"
    );
    assert!(
        text.contains("entry_points:"),
        "expected entry_points section"
    );
    assert!(
        text.contains("- src/main.rs"),
        "expected src/main.rs entry point"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
