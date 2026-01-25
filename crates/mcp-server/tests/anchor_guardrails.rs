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
async fn context_pack_anchor_missing_returns_no_unrelated_hits() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src/lib.rs"),
        "pub struct X11Window { pub id: u32 }\n\nimpl X11Window { pub fn new() -> Self { Self { id: 1 } } }\n",
    )
    .context("write src/lib.rs")?;

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

    let pack_args = serde_json::json!({
        "path": root.to_string_lossy(),
        "query": "LintWarning struct definition",
        "limit": 5,
        "max_chars": 4000,
        "response_mode": "full"
    });
    let pack = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "context_pack".into(),
            arguments: pack_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling context_pack")?
    .context("call context_pack")?;

    assert_ne!(pack.is_error, Some(true), "context_pack returned error");
    assert!(
        pack.structured_content.is_none(),
        "context_pack should not return structured_content"
    );
    let text = pack
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("context_pack missing text output")?;
    assert!(
        text.contains("context_pack: 0 items"),
        "expected zero items when anchor term is missing"
    );
    assert!(
        text.contains("no matches found"),
        "expected a no-matches note when anchor is missing"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
