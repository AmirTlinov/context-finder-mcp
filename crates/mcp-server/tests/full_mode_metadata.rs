use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RunningService, Service, ServiceExt},
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
        Duration::from_secs(30),
        service.call_tool(CallToolRequestParam {
            name: name.to_string().into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling tool")?
    .context("call tool")?;

    if result.is_error == Some(true) {
        let message = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap_or_else(|| "<no error payload>".to_string());
        anyhow::bail!("{name} returned error: {message}");
    }
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("tool did not return text content")?;
    Ok(text.to_string())
}

fn extract_root_fingerprint(text: &str) -> Result<u64> {
    let line = text
        .lines()
        .find(|line| line.starts_with("N: root_fingerprint="))
        .context("missing root_fingerprint note")?;
    let raw = line.trim_start_matches("N: root_fingerprint=").trim();
    raw.parse::<u64>().context("parse root_fingerprint")
}

#[tokio::test]
async fn context_pack_full_mode_includes_hit_metadata_notes() -> Result<()> {
    let service = start_mcp_server().await?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("lib.rs"),
        r#"
pub fn alpha() {
    let a = 1;
    let b = 2;
    let c = a + b;
    println!("{}", c);
}

pub fn beta() {
    alpha();
}
"#,
    )
    .context("write src/lib.rs")?;

    let text = call_tool_text(
        &service,
        "context_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "query": "alpha",
            "max_chars": 2500,
            "response_mode": "full",
        }),
    )
    .await?;

    anyhow::ensure!(
        text.starts_with("[CONTENT]\n") && !text.starts_with("[LEGEND]\n"),
        "expected full-mode output to stay low-noise (legend is provided by the help tool)"
    );
    anyhow::ensure!(
        text.contains("N: hit 1:"),
        "expected full-mode output to include per-hit metadata notes"
    );
    anyhow::ensure!(
        text.contains("role=") && text.contains("score="),
        "expected per-hit metadata to include role and score"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn context_pack_full_mode_includes_root_fingerprint_in_meta() -> Result<()> {
    let service = start_mcp_server().await?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("lib.rs"),
        r#"
pub fn alpha() {
    println!("root fp");
}
"#,
    )
    .context("write src/lib.rs")?;

    let text1 = call_tool_text(
        &service,
        "context_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "query": "alpha",
            "max_chars": 2500,
            "response_mode": "full",
        }),
    )
    .await?;
    let fp1 = extract_root_fingerprint(&text1)?;

    let text2 = call_tool_text(
        &service,
        "context_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "query": "alpha",
            "max_chars": 2500,
            "response_mode": "full",
        }),
    )
    .await?;
    let fp2 = extract_root_fingerprint(&text2)?;

    assert_eq!(fp1, fp2, "root fingerprint must be stable across calls");

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn search_full_mode_includes_hit_metadata_notes() -> Result<()> {
    let service = start_mcp_server().await?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("lib.rs"),
        r#"
pub fn alpha() {
    let mut out = String::new();
    out.push_str("hello");
    println!("{}", out);
}
"#,
    )
    .context("write src/lib.rs")?;

    let text = call_tool_text(
        &service,
        "search",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "query": "alpha",
            "limit": 5,
            "response_mode": "full",
        }),
    )
    .await?;

    anyhow::ensure!(
        text.starts_with("[CONTENT]\n") && !text.starts_with("[LEGEND]\n"),
        "expected full-mode output to stay low-noise (legend is provided by the help tool)"
    );
    anyhow::ensure!(
        text.contains("N: hit 1:"),
        "expected full-mode output to include per-hit metadata notes"
    );
    anyhow::ensure!(
        text.contains("score="),
        "expected per-hit metadata to include score"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
