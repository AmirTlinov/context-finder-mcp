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
async fn batch_v2_resolves_refs_between_items() -> Result<()> {
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
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(root.join("src").join("a.txt"), "TARGET\n").context("write a.txt")?;
    std::fs::write(root.join("src").join("z.txt"), "nope\n").context("write z.txt")?;

    let args = serde_json::json!({
        "version": 2,
        "path": root.to_string_lossy(),
        "max_chars": 20000,
        "items": [
            { "id": "files", "tool": "list_files", "input": { "file_pattern": "src/*", "limit": 10 } },
            { "id": "ctx", "tool": "grep_context", "input": { "pattern": "TARGET", "file": { "$ref": "#/items/files/data/files/0" }, "before": 0, "after": 0 } }
        ]
    });

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "batch".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling batch")??;

    assert_ne!(result.is_error, Some(true), "batch returned error");
    assert!(
        result.structured_content.is_none(),
        "batch should not return structured_content"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("batch missing text output")?;
    assert!(
        text.contains("item ctx: tool=grep_context status=ok"),
        "expected ctx item to succeed"
    );
    assert!(
        text.contains("src/a.txt"),
        "expected batch output to include src/a.txt"
    );
    assert!(
        text.contains("TARGET"),
        "expected batch output to include TARGET"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn batch_v2_accepts_action_payload_aliases() -> Result<()> {
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
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(root.join("src").join("a.txt"), "hello\n").context("write a.txt")?;

    let args = serde_json::json!({
        "version": 2,
        "path": root.to_string_lossy(),
        "max_chars": 20000,
        "items": [
            { "id": "files", "action": "list_files", "payload": { "file_pattern": "src/*", "limit": 10 } }
        ]
    });

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "batch".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling batch")??;

    assert_ne!(result.is_error, Some(true), "batch returned error");
    assert!(
        result.structured_content.is_none(),
        "batch should not return structured_content"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("batch missing text output")?;
    assert!(
        text.contains("item files: tool=list_files status=ok"),
        "expected files item to succeed"
    );
    assert!(
        text.contains("src/a.txt"),
        "expected batch output to include src/a.txt"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn batch_v2_respects_max_chars_budget() -> Result<()> {
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
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(root.join("src").join("a.txt"), "hello\n").context("write a.txt")?;

    let max_chars = 1200;
    let args = serde_json::json!({
        "version": 2,
        "path": root.to_string_lossy(),
        "max_chars": max_chars,
        "items": [
            { "id": "files", "tool": "list_files", "input": { "file_pattern": "src/*", "limit": 5 } }
        ]
    });

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "batch".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling batch")??;

    assert_ne!(result.is_error, Some(true), "batch returned error");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("batch did not return text content")?;
    assert!(
        text.chars().count() <= max_chars,
        "batch output exceeded max_chars"
    );
    assert!(
        result.structured_content.is_none(),
        "batch should not return structured_content"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn batch_v2_ref_to_failed_item_data_returns_error() -> Result<()> {
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
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(root.join("src").join("main.txt"), "hello\n").context("write main.txt")?;

    let args = serde_json::json!({
        "version": 2,
        "path": root.to_string_lossy(),
        "max_chars": 20000,
        "items": [
            { "id": "search", "tool": "text_search", "input": { "pattern": "   ", "file_pattern": "src/*" } },
            { "id": "slice", "tool": "file_slice", "input": { "file": { "$ref": "#/items/search/data/matches/0/file" }, "start_line": 1, "max_lines": 1 } }
        ]
    });

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "batch".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling batch")??;

    assert_ne!(result.is_error, Some(true), "batch returned error");
    assert!(
        result.structured_content.is_none(),
        "batch should not return structured_content"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("batch missing text output")?;
    assert!(
        text.contains("item slice: tool=file_slice status=error"),
        "expected slice item to error"
    );
    assert!(
        text.contains("Ref resolution error"),
        "expected ref resolution error in batch output"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
