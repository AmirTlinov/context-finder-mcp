use anyhow::{Context, Result};
use context_vector_store::context_dir_for_project_root;
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
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

async fn call_tool_text(
    service: &RunningService<RoleClient, ()>,
    name: &str,
    args: serde_json::Value,
) -> Result<String> {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: name.to_string().into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling tool")??;

    anyhow::ensure!(
        result.is_error != Some(true),
        "tool {name} returned error: {result:?}"
    );

    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .context("tool did not return text output")
}

#[tokio::test]
async fn read_tools_facts_mode_is_low_noise() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(root.join("README.md"), "hello\nworld\n").context("write README.md")?;
    std::fs::write(root.join("src").join("a.txt"), "one\nTARGET two\nthree\n")
        .context("write a.txt")?;
    std::fs::write(
        root.join("src").join("main.rs"),
        "fn main() { println!(\"needle\"); }\n",
    )
    .context("write main.rs")?;

    let context_dir = context_dir_for_project_root(root);
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "temp project unexpectedly has a context dir before read tools"
    );

    let cat = call_tool_text(
        &service,
        "cat",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "file": "README.md",
            "max_lines": 2,
            "max_chars": 2048,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert!(cat.starts_with("[CONTENT]\n"));

    let ls = call_tool_text(
        &service,
        "find",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "file_pattern": "src/*",
            "limit": 5,
            "max_chars": 4096,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert!(ls.starts_with("[CONTENT]\n"));

    let rg = call_tool_text(
        &service,
        "rg",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "TARGET",
            "file_pattern": "src/*",
            "context": 1,
            "max_chars": 4096,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert!(rg.starts_with("[CONTENT]\n"));

    let text_search = call_tool_text(
        &service,
        "text_search",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "needle",
            "max_results": 5,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert!(text_search.starts_with("[CONTENT]\n"));

    let tree = call_tool_text(
        &service,
        "tree",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "depth": 2,
            "limit": 10,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert!(tree.starts_with("[CONTENT]\n"));

    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "read tools created project context side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
