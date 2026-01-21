use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use std::path::{Path, PathBuf};
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
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "1");
    cmd.env(
        "CONTEXT_FINDER_MCP_SOCKET",
        socket.to_string_lossy().to_string(),
    );
    cmd
}

#[tokio::test]
async fn shared_backend_concurrent_startup_is_bounded() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

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
