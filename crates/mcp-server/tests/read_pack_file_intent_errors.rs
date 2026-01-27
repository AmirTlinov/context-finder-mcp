use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use serde_json::Value;
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
async fn read_pack_file_intent_reports_invalid_request_on_missing_file() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    let denied = call_tool(
        &service,
        "read_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "intent": "file",
            "file": "does_not_exist.txt",
            "max_chars": 2000,
        }),
    )
    .await?;
    assert_error_code(&denied, "invalid_request")?;

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
