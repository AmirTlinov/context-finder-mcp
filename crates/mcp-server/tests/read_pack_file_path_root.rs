use anyhow::{Context, Result};
use context_vector_store::context_dir_for_project_root;
use rmcp::{model::CallToolRequestParam, service::ServiceExt, transport::TokioChildProcess};
use std::time::Duration;
use tokio::process::Command;

mod support;

#[tokio::test]
async fn read_pack_memory_accepts_file_path_as_root_hint() -> Result<()> {
    let bin = support::locate_context_mcp_bin()?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::write(
        root.join("AGENTS.context"),
        "[LEGEND]\n[CONTENT]\nA: hello\n",
    )
    .context("write AGENTS.context")?;
    std::fs::write(
        root.join("PHILOSOPHY.context"),
        "[LEGEND]\n[CONTENT]\nA: philosophy\n",
    )
    .context("write PHILOSOPHY.context")?;

    let context_dir = context_dir_for_project_root(root);
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "temp project unexpectedly has a context dir before read_pack"
    );

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let philosophy_file = root.join("PHILOSOPHY.context");
    let args = serde_json::json!({
        "path": philosophy_file.to_string_lossy(),
        "intent": "memory",
        "max_chars": 4_000,
        "response_mode": "facts",
    });

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "read_pack".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling read_pack")??;

    assert_ne!(result.is_error, Some(true), "read_pack returned error");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("read_pack missing text output")?;
    assert!(
        text.contains("PHILOSOPHY.context"),
        "expected read_pack to resolve file path root and anchor PHILOSOPHY.context"
    );
    assert!(
        text.contains("AGENTS.context"),
        "expected read_pack memory intent to discover AGENTS.context"
    );
    assert!(
        text.contains("R:"),
        "expected at least one reference anchor"
    );

    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "read_pack created project context side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn read_pack_memory_surfaces_focus_file_when_root_is_set_by_file_path() -> Result<()> {
    let bin = support::locate_context_mcp_bin()?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::write(
        root.join("AGENTS.context"),
        "[LEGEND]\n[CONTENT]\nA: hello\n",
    )
    .context("write AGENTS.context")?;
    std::fs::create_dir_all(root.join(".git")).context("create .git")?;
    std::fs::create_dir_all(root.join("src")).context("create src")?;
    std::fs::write(
        root.join("src").join("focus.rs"),
        "fn focus() {\n  println!(\"hi\");\n}\n",
    )
    .context("write focus.rs")?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let focus_file = root.join("src").join("focus.rs");
    let args = serde_json::json!({
        "path": focus_file.to_string_lossy(),
        "intent": "memory",
        "max_chars": 4_000,
        "response_mode": "facts",
    });

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "read_pack".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling read_pack")??;

    assert_ne!(result.is_error, Some(true), "read_pack returned error");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("read_pack missing text output")?;
    assert!(
        text.contains("src/focus.rs"),
        "expected memory pack to surface the focus file when `path` is a file hint"
    );
    assert!(
        text.contains("AGENTS.context"),
        "expected memory pack to keep stable doc anchors alongside the focus file"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
