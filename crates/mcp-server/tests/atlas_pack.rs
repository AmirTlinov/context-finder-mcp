use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RunningService, Service, ServiceExt},
    transport::TokioChildProcess,
};
use std::path::{Path, PathBuf};
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

async fn start_mcp_server(
) -> Result<RunningService<rmcp::RoleClient, impl Service<rmcp::RoleClient>>> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

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

#[tokio::test]
async fn atlas_pack_includes_meaning_cp_and_worktrees() -> Result<()> {
    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    git(root, &["init"]).await?;
    git(root, &["config", "user.email", "test@example.com"]).await?;
    git(root, &["config", "user.name", "Test"]).await?;

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::create_dir_all(root.join(".github").join("workflows"))
        .context("mkdir .github/workflows")?;
    std::fs::create_dir_all(root.join("contracts").join("command").join("v1"))
        .context("mkdir contracts/command/v1")?;

    std::fs::write(
        root.join("src").join("main.rs"),
        "fn main() { println!(\"ok\"); }\n",
    )
    .context("write src/main.rs")?;
    std::fs::write(
        root.join(".github").join("workflows").join("ci.yml"),
        r#"name: CI
on:
  push:
jobs:
  gates:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: test
        run: cargo test --workspace
"#,
    )
    .context("write .github/workflows/ci.yml")?;
    std::fs::write(
        root.join("contracts")
            .join("command")
            .join("v1")
            .join("envelope.json"),
        r#"{"type":"object","properties":{"kind":{"type":"string"}}}"#,
    )
    .context("write contracts/command/v1/envelope.json")?;

    git(root, &["add", "."]).await?;
    git(root, &["commit", "-m", "init"]).await?;
    let _ = git(root, &["branch", "-M", "main"]).await;

    let w1 = root.join(".worktrees").join("w1");
    git(
        root,
        &[
            "worktree",
            "add",
            "-b",
            "feature-w1",
            w1.to_string_lossy().as_ref(),
        ],
    )
    .await?;
    std::fs::write(w1.join("WIP.txt"), "wip\n").context("write worktree WIP")?;

    let service = start_mcp_server().await?;
    let text = call_tool_text(
        &service,
        "atlas_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "response_mode": "facts",
            "max_chars": 4000
        }),
    )
    .await?;

    assert!(text.contains("atlas_pack"), "expected tool name in output");
    assert!(text.contains("CPV1"), "expected meaning CPV1 pack");
    assert!(
        text.contains(".github/workflows/ci.yml"),
        "expected CI workflow path in meaning pack"
    );
    assert!(
        text.contains("contracts/command/v1/envelope.json"),
        "expected contracts path in meaning pack"
    );
    assert!(
        text.contains(".worktrees/w1") || text.contains(".worktrees\\w1"),
        "expected worktree path in output"
    );

    let full = call_tool_text(
        &service,
        "atlas_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "response_mode": "full",
            "max_chars": 4000
        }),
    )
    .await?;
    assert!(
        full.contains("next_actions:"),
        "expected next_actions in full mode output"
    );
    assert!(
        full.contains("tool=worktree_pack") || full.contains("\"worktree_pack\""),
        "expected worktree_pack drill-down hint in full mode"
    );
    assert!(
        full.contains("tool=meaning_pack") || full.contains("\"meaning_pack\""),
        "expected meaning_pack drill-down hint for best worktree in full mode"
    );

    Ok(())
}
