use anyhow::{Context, Result};
use rmcp::{model::CallToolRequestParam, service::ServiceExt, transport::TokioChildProcess};
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

#[tokio::test]
async fn error_text_includes_root_fingerprint_when_meta_is_attached() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

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
