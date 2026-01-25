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
async fn impact_falls_back_to_text_matches_when_graph_has_no_edges() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("lib.rs"),
        "macro_rules! foo { () => {} }\nfn caller() { foo!(); }\n",
    )
    .context("write lib.rs")?;

    let impact_args = serde_json::json!({
        "symbol": "foo",
        "path": root.to_string_lossy(),
        "depth": 2,
        "language": "rust",
    });
    let impact_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "impact".into(),
            arguments: impact_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling impact")??;

    if impact_result.is_error == Some(true) {
        let message = impact_result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap_or_else(|| "<no text error payload>".to_string());
        panic!("impact returned error: {message}");
    }
    let text = impact_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("impact missing text output")?;
    assert!(
        text.contains("TextMatch"),
        "expected TextMatch usage in impact output"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn impact_returns_text_matches_when_symbol_is_missing_from_graph() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("lib.rs"),
        "fn caller() {\n  let x = FOO_UNKNOWN + 1;\n}\n",
    )
    .context("write src/lib.rs")?;

    let impact_args = serde_json::json!({
        "symbol": "FOO_UNKNOWN",
        "path": root.to_string_lossy(),
        "depth": 2,
        "language": "rust",
    });
    let impact_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "impact".into(),
            arguments: impact_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling impact")??;

    if impact_result.is_error == Some(true) {
        let message = impact_result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap_or_else(|| "<no text error payload>".to_string());
        panic!("impact returned error: {message}");
    }
    let text = impact_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("impact missing text output")?;
    assert!(
        text.contains("TextMatch"),
        "expected TextMatch usage in impact output"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn impact_finds_symbols_only_mentioned_in_docs_via_filesystem_fallback() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("docs")).context("mkdir docs")?;
    std::fs::write(
        root.join("docs").join("adr-0001.md"),
        "# ADR\n\nWe introduce PerceptualLint for better DX.\n",
    )
    .context("write docs/adr-0001.md")?;

    let impact_args = serde_json::json!({
        "symbol": "PerceptualLint",
        "path": root.to_string_lossy(),
        "depth": 2,
        "language": "rust",
    });
    let impact_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "impact".into(),
            arguments: impact_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling impact")??;

    if impact_result.is_error == Some(true) {
        let message = impact_result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap_or_else(|| "<no text error payload>".to_string());
        panic!("impact returned error: {message}");
    }

    assert!(
        impact_result.structured_content.is_none(),
        "impact should not return structured_content"
    );
    let text = impact_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("impact missing text output")?;
    assert!(
        text.contains("docs/adr-0001.md"),
        "expected impact fallback to reference docs/adr-0001.md"
    );
    assert!(
        text.contains("TextMatch"),
        "expected impact to surface a TextMatch usage"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
