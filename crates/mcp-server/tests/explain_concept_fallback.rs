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
async fn explain_falls_back_to_docs_concept_when_symbol_is_not_in_graph() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::create_dir_all(root.join("docs")).context("mkdir docs")?;
    std::fs::write(root.join("src/lib.rs"), "pub fn main() {}\n").context("write src/lib.rs")?;
    std::fs::write(
        root.join("docs/adr-0001.md"),
        "# ADR\n\nWe introduce PerceptualLint to avoid regressions.\n",
    )
    .context("write docs/adr-0001.md")?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");
    cmd.env("RUST_LOG", "warn");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let explain_args = serde_json::json!({
        "path": root.to_string_lossy(),
        "symbol": "PerceptualLint",
        "response_mode": "facts"
    });
    let explain = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "explain".into(),
            arguments: explain_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling explain")?
    .context("call explain")?;

    assert_ne!(explain.is_error, Some(true), "explain returned error");
    assert!(
        explain.structured_content.is_none(),
        "explain should not return structured_content"
    );
    let text = explain
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("explain missing text output")?;
    assert!(
        text.contains("docs/adr-0001.md"),
        "expected explain fallback to reference docs/adr-0001.md"
    );
    assert!(
        text.contains("PerceptualLint"),
        "expected explain fallback content to mention PerceptualLint"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
