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
async fn repo_onboarding_pack_returns_map_docs_and_next_actions() -> Result<()> {
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
    std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").context("write main.rs")?;

    std::fs::create_dir_all(root.join("docs")).context("mkdir docs")?;
    std::fs::write(root.join("README.md"), "# Hello\n").context("write README.md")?;
    std::fs::write(root.join("docs").join("README.md"), "# Docs\n")
        .context("write docs/README.md")?;

    assert!(
        !root.join(".context-finder").exists(),
        "temp project unexpectedly has .context-finder before repo_onboarding_pack"
    );

    let args = serde_json::json!({
        "path": root.to_string_lossy(),
        "map_depth": 2,
        "map_limit": 10,
        "docs_limit": 5,
        "doc_max_lines": 50,
        "doc_max_chars": 2000,
        "max_chars": 20000,
    });
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "repo_onboarding_pack".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling repo_onboarding_pack")??;

    assert_ne!(
        result.is_error,
        Some(true),
        "repo_onboarding_pack returned error"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("repo_onboarding_pack did not return text content")?;
    let json: Value =
        serde_json::from_str(text).context("repo_onboarding_pack output is not valid JSON")?;

    assert_eq!(json.get("version").and_then(Value::as_u64), Some(1));

    let map = json.get("map").context("missing map")?;
    assert!(
        map.get("directories").and_then(Value::as_array).is_some(),
        "map.directories missing"
    );

    let docs = json
        .get("docs")
        .and_then(Value::as_array)
        .context("missing docs array")?;
    assert!(
        docs.iter()
            .any(|d| d.get("file").and_then(Value::as_str) == Some("README.md")),
        "expected README.md in docs slices"
    );

    let next_actions = json
        .get("next_actions")
        .and_then(Value::as_array)
        .context("missing next_actions array")?;
    assert!(!next_actions.is_empty(), "expected non-empty next_actions");

    let budget = json.get("budget").context("missing budget")?;
    let max_chars = budget
        .get("max_chars")
        .and_then(Value::as_u64)
        .context("budget.max_chars missing")?;
    let used_chars = budget
        .get("used_chars")
        .and_then(Value::as_u64)
        .context("budget.used_chars missing")?;
    assert!(used_chars <= max_chars);

    assert!(
        !root.join(".context-finder").exists(),
        "repo_onboarding_pack created .context-finder side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn repo_onboarding_pack_clamps_tiny_budget_and_keeps_next_actions() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").context("write main.rs")?;
    std::fs::write(root.join("README.md"), "# Hello\n").context("write README.md")?;

    let args = serde_json::json!({
        "path": root.to_string_lossy(),
        "map_depth": 2,
        "docs_limit": 3,
        "doc_max_lines": 10,
        "doc_max_chars": 200,
        "max_chars": 5,
    });
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "repo_onboarding_pack".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling repo_onboarding_pack")??;

    assert_ne!(
        result.is_error,
        Some(true),
        "repo_onboarding_pack returned error"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("repo_onboarding_pack did not return text content")?;
    let json: Value =
        serde_json::from_str(text).context("repo_onboarding_pack output is not valid JSON")?;

    let budget = json.get("budget").context("missing budget")?;
    let max_chars = budget
        .get("max_chars")
        .and_then(Value::as_u64)
        .context("budget.max_chars missing")?;
    assert!(max_chars >= 1000, "expected min budget clamp");

    let next_actions = json
        .get("next_actions")
        .and_then(Value::as_array)
        .context("missing next_actions array")?;
    assert!(!next_actions.is_empty(), "expected next_actions");

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
