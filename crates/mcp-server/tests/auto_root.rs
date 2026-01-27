use anyhow::{Context, Result};
use context_vector_store::context_dir_for_project_root;
use rmcp::{model::CallToolRequestParam, service::ServiceExt, transport::TokioChildProcess};
use std::time::Duration;
use tokio::process::Command;

mod support;

#[tokio::test]
async fn find_uses_env_root_when_path_missing() -> Result<()> {
    let bin = support::locate_context_mcp_bin()?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(root.join("src").join("a.rs"), "fn a() {}\n").context("write a.rs")?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_ROOT", root);

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let args = serde_json::json!({
        "file_pattern": "src/*",
        "limit": 10,
        "max_chars": 20_000,
    });
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "find".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling find")??;

    assert_ne!(result.is_error, Some(true), "find returned error");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("find missing text output")?;
    assert!(
        text.contains("src/a.rs"),
        "expected src/a.rs in list_files output"
    );

    let context_dir = context_dir_for_project_root(root);
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "find created project context side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
