use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use serde_json::Value;
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

async fn start_service() -> Result<(tempfile::TempDir, RunningService<RoleClient, ()>)> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tmp = tempfile::tempdir().context("tempdir")?;
    Ok((tmp, service))
}

async fn call_tool(
    service: &RunningService<RoleClient, ()>,
    name: &str,
    args: Value,
) -> Result<rmcp::model::CallToolResult> {
    tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: name.to_string().into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling tool")?
    .context("tool call failed")
}

fn assert_error_code(result: &rmcp::model::CallToolResult, expected: &str) -> Result<()> {
    assert_eq!(result.is_error, Some(true));
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("error missing text content")?;
    assert!(
        text.contains(&format!("A: error: {expected}")),
        "expected error code {expected}, got:\n{text}"
    );
    Ok(())
}

#[tokio::test]
async fn cat_refuses_secret_by_default_but_allows_opt_in() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::write(root.join(".env"), "SECRET=1\n").context("write .env")?;

    let denied = call_tool(
        &service,
        "cat",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "file": ".env",
            "max_lines": 5,
            "max_chars": 2000,
        }),
    )
    .await?;
    assert_error_code(&denied, "invalid_request")?;

    let allowed = call_tool(
        &service,
        "cat",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "file": ".env",
            "max_lines": 5,
            "max_chars": 2000,
            "allow_secrets": true,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(allowed.is_error, Some(true));
    let text = allowed
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("cat missing text content")?;
    assert!(text.contains("R: .env:1 file slice"));
    assert!(text.contains("SECRET=1"));

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn rg_refuses_secret_file_by_default_but_allows_opt_in() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::write(root.join(".env"), "SECRET=1\nOTHER=2\n").context("write .env")?;

    let denied = call_tool(
        &service,
        "rg",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "SECRET",
            "file": ".env",
            "before": 0,
            "after": 0,
            "max_hunks": 5,
            "max_chars": 2000,
        }),
    )
    .await?;
    assert_error_code(&denied, "invalid_request")?;

    let allowed = call_tool(
        &service,
        "rg",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "SECRET",
            "file": ".env",
            "before": 0,
            "after": 0,
            "max_hunks": 5,
            "max_chars": 2000,
            "allow_secrets": true,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(allowed.is_error, Some(true));
    let text = allowed
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("rg missing text content")?;
    assert!(text.contains("R: .env:1 grep hunk"));
    assert!(text.contains("SECRET=1"));

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn text_search_skips_secret_files_by_default_but_allows_opt_in() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::write(root.join(".env"), "SECRET=1\nOTHER=2\n").context("write .env")?;
    std::fs::write(root.join("safe.txt"), "SECRET=ok\n").context("write safe.txt")?;

    let default_res = call_tool(
        &service,
        "text_search",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "SECRET",
            "max_results": 10,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(default_res.is_error, Some(true));
    let text = default_res
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("text_search missing text content")?;
    assert!(
        !text.contains("R: .env:"),
        "default text_search should not include .env matches"
    );
    assert!(
        text.contains("R: safe.txt:1 matches"),
        "default text_search should include safe.txt match"
    );

    let allow_res = call_tool(
        &service,
        "text_search",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "SECRET",
            "max_results": 10,
            "allow_secrets": true,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(allow_res.is_error, Some(true));
    let text = allow_res
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("text_search missing text content")?;
    assert!(
        text.contains("R: .env:1 matches"),
        "allow_secrets text_search should include .env matches"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn read_pack_file_intent_refuses_secret_by_default_but_allows_opt_in() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::write(root.join(".env"), "SECRET=1\n").context("write .env")?;

    let denied = call_tool(
        &service,
        "read_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "intent": "file",
            "file": ".env",
            "max_chars": 4000,
        }),
    )
    .await?;
    assert_error_code(&denied, "forbidden_file")?;

    let allowed = call_tool(
        &service,
        "read_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "intent": "file",
            "file": ".env",
            "max_chars": 4000,
            "allow_secrets": true,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(allowed.is_error, Some(true));
    let text = allowed
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("read_pack missing text content")?;
    assert!(text.contains("R: .env:1"));
    assert!(text.contains("SECRET=1"));

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
