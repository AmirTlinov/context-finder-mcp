use anyhow::{Context, Result};
use context_vector_store::context_dir_for_project_root;
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

    anyhow::bail!("failed to locate context-finder-mcp binary");
}

#[tokio::test]
async fn context_pack_auto_indexes_missing_project() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");

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

    let context_dir = context_dir_for_project_root(root);
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "temp project unexpectedly has a context dir before context_pack"
    );

    let args = serde_json::json!({
        "path": root.to_string_lossy(),
        "query": "alpha",
        "max_chars": 5000,
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
        .context("context_pack missing text output")?;
    assert!(
        text.contains("[CONTENT]"),
        "context_pack must return `.context` output"
    );

    let indexes_dir = context_dir_for_project_root(root).join("indexes");
    assert!(
        indexes_dir.exists(),
        "context_pack did not create project context indexes dir"
    );
    let mut found_index_json = false;
    for entry in std::fs::read_dir(&indexes_dir).context("read indexes dir")? {
        let entry = entry?;
        let candidate = entry.path().join("index.json");
        if candidate.exists() {
            found_index_json = true;
            break;
        }
    }
    assert!(
        found_index_json,
        "context_pack did not write any index.json files"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
