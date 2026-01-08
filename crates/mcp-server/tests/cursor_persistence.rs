use anyhow::{Context, Result};
use rmcp::{model::CallToolRequestParam, service::ServiceExt, transport::TokioChildProcess};
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

async fn spawn_server(
    bin: &Path,
    cursor_store_path: &Path,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>> {
    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    cmd.env(
        "CONTEXT_FINDER_MCP_CURSOR_STORE_PATH",
        cursor_store_path.to_string_lossy().to_string(),
    );

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")
}

#[tokio::test]
async fn cursor_alias_survives_process_restart() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let cursor_store_dir = tempfile::tempdir().context("temp cursor store dir")?;
    let cursor_store_path = cursor_store_dir.path().join("cursor_store.json");

    let tmp = tempfile::tempdir().context("temp project dir")?;
    let root = tmp.path();
    std::fs::write(root.join("README.md"), "one\ntwo\nthree\n").context("write README.md")?;

    // First server: request a paginated slice to obtain a compact cursor alias.
    let service_a = spawn_server(&bin, &cursor_store_path).await?;
    let args_a = serde_json::json!({
        "path": root.to_string_lossy(),
        "file": "README.md",
        "max_lines": 1,
        "max_chars": 2048,
        "response_mode": "minimal",
    });
    let resp_a = tokio::time::timeout(
        Duration::from_secs(10),
        service_a.call_tool(CallToolRequestParam {
            name: "file_slice".into(),
            arguments: args_a.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling file_slice on server A")??;

    let text_a = resp_a
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("missing file_slice text output from server A")?;
    let cursor = text_a
        .lines()
        .find_map(|line| line.strip_prefix("M: ").map(str::to_string))
        .context("missing next_cursor (M:) line in file_slice response A")?;
    assert!(
        cursor.starts_with("cfcs1:"),
        "expected compact cursor alias, got: {cursor:?}"
    );

    service_a.cancel().await.context("shutdown server A")?;

    // Second server: use the same cursor alias after restart, proving persistence.
    let service_b = spawn_server(&bin, &cursor_store_path).await?;
    let args_b = serde_json::json!({
        "path": root.to_string_lossy(),
        "cursor": cursor,
        "max_chars": 2048,
        "response_mode": "minimal",
    });
    let resp_b = tokio::time::timeout(
        Duration::from_secs(10),
        service_b.call_tool(CallToolRequestParam {
            name: "file_slice".into(),
            arguments: args_b.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling file_slice on server B")??;

    let text_b = resp_b
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("missing file_slice text output from server B")?;
    assert!(
        text_b.contains("two"),
        "expected second page to include \"two\""
    );

    service_b.cancel().await.context("shutdown server B")?;
    Ok(())
}
