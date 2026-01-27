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

async fn call_cat_once(
    service: &RunningService<RoleClient, impl rmcp::service::Service<RoleClient>>,
    max_chars: usize,
) -> Result<()> {
    let args = serde_json::json!({
        "file": "README.md",
        "max_lines": 1,
        "max_chars": max_chars,
    });
    let resp = tokio::time::timeout(
        Duration::from_secs(8),
        service.call_tool(CallToolRequestParam {
            name: "cat".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling cat")?
    .context("call cat")?;

    anyhow::ensure!(resp.is_error != Some(true), "cat returned error: {resp:?}");

    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("cat missing text output")?;
    assert!(text.contains("\nhello\n"), "expected to read first line");
    assert!(!text.contains("world"), "expected max_lines=1");

    Ok(())
}

fn spawn_proxy_cmd(bin: &Path, socket: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.current_dir(cwd);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_MCP_SHARED", "1");
    cmd.env("CONTEXT_MCP_SOCKET", socket.to_string_lossy().to_string());
    cmd
}

#[tokio::test]
async fn shared_backend_concurrent_startup_is_bounded() -> Result<()> {
    let bin = support::locate_context_mcp_bin()?;

    let socket_dir = tempfile::tempdir().context("tempdir for mcp daemon socket")?;
    let socket = socket_dir.path().join("mcp.sock");

    let tmp = tempfile::tempdir().context("temp project dir")?;
    let root = tmp.path();
    std::fs::write(root.join("README.md"), "hello\nworld\n").context("write README.md")?;

    // Spawn multiple proxies concurrently against the same socket to exercise startup races.
    let concurrency = 8usize;
    let mut tasks = Vec::new();
    for _ in 0..concurrency {
        let cmd = spawn_proxy_cmd(&bin, &socket, root);
        let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
        tasks.push(tokio::spawn(async move {
            let service = tokio::time::timeout(Duration::from_secs(8), ().serve(transport))
                .await
                .context("timeout starting shared-backend MCP proxy")?
                .context("start shared-backend MCP proxy")?;

            call_cat_once(&service, 2048).await?;
            service.cancel().await.context("shutdown proxy service")?;
            Ok::<(), anyhow::Error>(())
        }));
    }

    for task in tasks {
        task.await.context("join proxy task")??;
    }

    Ok(())
}
