use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RunningService, Service, ServiceExt},
    transport::TokioChildProcess,
};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

mod support;

async fn start_mcp_server(
) -> Result<RunningService<rmcp::RoleClient, impl Service<rmcp::RoleClient>>> {
    let bin = support::locate_context_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")
}

async fn call_tool_text(
    service: &RunningService<rmcp::RoleClient, impl Service<rmcp::RoleClient>>,
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
    .context("timeout calling tool")?
    .context("call tool")?;

    assert_ne!(result.is_error, Some(true), "{name} returned error");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("tool did not return text content")?;
    Ok(text.to_string())
}

async fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .await
        .with_context(|| format!("run git {:?}", args))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn init_min_repo() -> Result<(tempfile::TempDir, PathBuf)> {
    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path().to_path_buf();

    git(&root, &["init"]).await?;
    git(&root, &["config", "user.email", "test@example.com"]).await?;
    git(&root, &["config", "user.name", "Test"]).await?;

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::create_dir_all(root.join(".github").join("workflows")).context("mkdir workflows")?;

    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "demo-coverage"
version = "0.1.0"
edition = "2021"

[dependencies]
"#,
    )
    .context("write Cargo.toml")?;
    std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").context("write main")?;
    std::fs::write(
        root.join(".github").join("workflows").join("ci.yml"),
        "name: CI\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: cargo test --workspace\n",
    )
    .context("write ci.yml")?;

    git(&root, &["add", "."]).await?;
    git(&root, &["commit", "-m", "init"]).await?;
    Ok((tmp, root))
}

#[tokio::test]
async fn meaning_pack_emits_coverage_hint_in_facts_mode() -> Result<()> {
    let (_tmp, root) = init_min_repo().await?;
    let service = start_mcp_server().await?;

    let text = call_tool_text(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "query": "canon loop",
            "response_mode": "facts",
            "max_chars": 4000
        }),
    )
    .await?;

    assert!(
        text.contains("coverage: anchors_ev=")
            && text.contains("steps_ev=")
            && text.contains("ev="),
        "expected coverage hint in meaning_pack facts mode (got: {text})"
    );
    Ok(())
}

#[tokio::test]
async fn atlas_pack_emits_meaning_coverage_hint_in_facts_mode() -> Result<()> {
    let (_tmp, root) = init_min_repo().await?;
    let service = start_mcp_server().await?;

    let text = call_tool_text(
        &service,
        "atlas_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "response_mode": "facts",
            "max_chars": 5000
        }),
    )
    .await?;

    assert!(
        text.contains("meaning_coverage: anchors_ev=")
            && text.contains("steps_ev=")
            && text.contains("ev="),
        "expected meaning_coverage hint in atlas_pack facts mode (got: {text})"
    );
    Ok(())
}
