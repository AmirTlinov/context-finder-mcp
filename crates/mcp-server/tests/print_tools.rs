use anyhow::{Context, Result};
use rmcp::{service::ServiceExt, transport::TokioChildProcess};
use serde_json::Value;
use std::collections::HashSet;
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
async fn print_tools_matches_list_tools() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let output = Command::new(&bin)
        .arg("--print-tools")
        .output()
        .await
        .context("run context-finder-mcp --print-tools")?;
    assert!(
        output.status.success(),
        "print-tools failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).context("decode print-tools stdout")?;
    let payload: Value = serde_json::from_str(&stdout).context("parse print-tools JSON")?;
    assert_eq!(
        payload
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        env!("CARGO_PKG_VERSION"),
        "print-tools version mismatch"
    );

    let printed_tools = payload
        .get("tools")
        .and_then(Value::as_array)
        .context("print-tools missing tools array")?;
    let printed: HashSet<String> = printed_tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(|name| name.to_string())
        .collect();
    assert!(!printed.is_empty(), "print-tools returned no tools");

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tools = tokio::time::timeout(
        Duration::from_secs(10),
        service.list_tools(Default::default()),
    )
    .await
    .context("timeout listing tools")??;
    let listed: HashSet<String> = tools
        .tools
        .iter()
        .map(|t| t.name.as_ref().to_string())
        .collect();

    assert_eq!(printed, listed, "print-tools mismatch with list_tools");
    Ok(())
}
