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

    anyhow::bail!("failed to locate context-finder-mcp binary");
}

#[tokio::test]
async fn context_pack_auto_indexes_missing_project() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("lib.rs"),
        "pub fn alpha() { println!(\"auto-index\"); }\n",
    )
    .context("write lib.rs")?;

    assert!(
        !root.join(".context-finder").exists(),
        "temp project unexpectedly has .context-finder before context_pack"
    );

    let args = serde_json::json!({
        "path": root.to_string_lossy(),
        "query": "alpha",
        "max_chars": 5000,
        "auto_index": true,
        "auto_index_budget_ms": 5000,
    });
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        service.call_tool(CallToolRequestParam {
            name: "context_pack".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling context_pack")??;

    assert_ne!(result.is_error, Some(true), "context_pack returned error");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("context_pack did not return text content")?;
    let json: Value =
        serde_json::from_str(text).context("context_pack output is not valid JSON")?;

    let index_state = json
        .get("meta")
        .and_then(|m| m.get("index_state"))
        .context("context_pack meta.index_state missing")?;
    let index_exists = index_state
        .get("index")
        .and_then(|i| i.get("exists"))
        .and_then(Value::as_bool);
    assert_eq!(index_exists, Some(true));

    let reindex = index_state
        .get("reindex")
        .context("context_pack meta.index_state.reindex missing")?;
    assert_eq!(
        reindex.get("attempted").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        reindex.get("performed").and_then(Value::as_bool),
        Some(true)
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
