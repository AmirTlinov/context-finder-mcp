use anyhow::{Context, Result};
use rmcp::{model::CallToolRequestParam, service::ServiceExt, transport::TokioChildProcess};
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

#[tokio::test]
async fn batch_v2_resolves_refs_between_items() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
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
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("batch did not return text content")?;
    let json: Value = serde_json::from_str(text).context("batch output is not valid JSON")?;

    assert_eq!(json.get("version").and_then(Value::as_u64), Some(2));
    let items = json
        .get("items")
        .and_then(Value::as_array)
        .context("batch items missing")?;
    assert_eq!(items.len(), 2);

    let ctx_item = items
        .iter()
        .find(|v| v.get("id").and_then(Value::as_str) == Some("ctx"))
        .context("missing ctx item")?;
    assert_eq!(ctx_item.get("status").and_then(Value::as_str), Some("ok"));

    let ctx_data = ctx_item.get("data").context("ctx item missing data")?;
    assert_eq!(
        ctx_data.get("file").and_then(Value::as_str),
        Some("src/a.txt")
    );
    let hunks = ctx_data
        .get("hunks")
        .and_then(Value::as_array)
        .context("ctx data missing hunks")?;
    assert!(!hunks.is_empty(), "expected at least one hunk");
    let content = hunks[0]
        .get("content")
        .and_then(Value::as_str)
        .context("hunk missing content")?;
    assert!(content.contains("TARGET"));

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
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("batch did not return text content")?;
    let json: Value = serde_json::from_str(text).context("batch output is not valid JSON")?;

    let items = json
        .get("items")
        .and_then(Value::as_array)
        .context("batch items missing")?;
    let files_item = items.first().context("missing files item")?;
    assert_eq!(files_item.get("status").and_then(Value::as_str), Some("ok"));
    let data = files_item.get("data").context("files item missing data")?;
    let files = data
        .get("files")
        .and_then(Value::as_array)
        .context("files data missing files array")?;
    assert!(files.iter().any(|v| v.as_str() == Some("src/a.txt")));

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
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("batch did not return text content")?;
    let json: Value = serde_json::from_str(text).context("batch output is not valid JSON")?;

    let items = json
        .get("items")
        .and_then(Value::as_array)
        .context("batch items missing")?;

    let slice_item = items
        .iter()
        .find(|v| v.get("id").and_then(Value::as_str) == Some("slice"))
        .context("missing slice item")?;
    assert_eq!(
        slice_item.get("status").and_then(Value::as_str),
        Some("error")
    );
    let message = slice_item
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        message.contains("Ref resolution error"),
        "expected ref resolution error, got: {message}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
