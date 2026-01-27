use anyhow::{Context, Result};
use rmcp::{model::CallToolRequestParam, service::ServiceExt, transport::TokioChildProcess};
use std::time::Duration;
use tokio::process::Command;

mod support;

#[tokio::test]
async fn error_text_includes_root_fingerprint_when_meta_is_attached() -> Result<()> {
    let bin = support::locate_context_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    std::fs::write(tmp.path().join("README.md"), "# hello\n").context("write README.md")?;

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "context_pack".into(),
            arguments: serde_json::json!({
                "path": tmp.path().to_string_lossy(),
                "query": "",
                "max_chars": 2000,
                "response_mode": "facts",
            })
            .as_object()
            .cloned(),
        }),
    )
    .await
    .context("timeout calling context_pack")?
    .context("call context_pack")?;

    assert_eq!(
        result.is_error,
        Some(true),
        "expected context_pack to error on empty query"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or("");
    assert!(
        text.contains("root_fingerprint="),
        "expected error text to include root_fingerprint note"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
