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

    anyhow::bail!("failed to locate context-finder-mcp binary")
}

async fn call_tool_text(
    service: &rmcp::service::RunningService<
        rmcp::RoleClient,
        impl rmcp::service::Service<rmcp::RoleClient>,
    >,
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

fn assert_is_context_doc(text: &str) {
    assert!(
        text.contains("[CONTENT]"),
        "context payload must contain a [CONTENT] marker"
    );
    assert!(
        text.contains("A:"),
        "context payload must include at least one answer line (A:)"
    );
}

#[tokio::test]
async fn repo_onboarding_pack_returns_map_docs_and_next_actions() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
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

    std::fs::create_dir_all(root.join("docs")).context("mkdir docs")?;
    std::fs::write(root.join("README.md"), "# Hello\n").context("write README.md")?;
    std::fs::write(root.join("docs").join("README.md"), "# Docs\n")
        .context("write docs/README.md")?;

    let context_dir = context_dir_for_project_root(root);
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "temp project unexpectedly has a context dir before repo_onboarding_pack"
    );

    let text = call_tool_text(
        &service,
        "repo_onboarding_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "map_depth": 2,
            "map_limit": 10,
            "docs_limit": 5,
            "doc_max_lines": 50,
            "doc_max_chars": 2000,
            "max_chars": 20000,
            "response_mode": "full"
        }),
    )
    .await?;

    assert_is_context_doc(&text);
    assert!(
        text.starts_with("[CONTENT]\n"),
        "expected `.context` payload to be low-noise (legend is provided by the help tool)"
    );
    assert!(
        text.contains("A: repo_onboarding_pack:"),
        "expected answer line describing the tool result"
    );
    assert!(text.contains("N: map:"), "expected map section marker");
    assert!(
        text.lines().any(|line| line.trim() == "src"),
        "expected src in map output"
    );
    assert!(text.contains("N: docs:"), "expected docs section marker");
    assert!(
        text.contains("R: README.md:1 doc"),
        "expected README.md doc slice reference"
    );
    assert!(
        text.contains("R: docs/README.md:1 doc"),
        "expected docs/README.md doc slice reference"
    );

    assert!(
        context_dir_for_project_root(root).exists(),
        "repo_onboarding_pack should auto-refresh the index"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn repo_onboarding_pack_reports_docs_reason_when_docs_disabled() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::write(root.join("README.md"), "# Hello\n").context("write README.md")?;

    let text = call_tool_text(
        &service,
        "repo_onboarding_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "docs_limit": 0
        }),
    )
    .await?;

    assert_is_context_doc(&text);
    assert!(
        text.starts_with("[CONTENT]\n"),
        "default output should be low-noise `.context` (no legend)"
    );
    assert!(
        text.contains("docs=0"),
        "expected docs=0 summary when docs_limit=0"
    );
    assert!(
        !text.contains("\nR:"),
        "expected no doc slice references when docs_limit=0"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn repo_onboarding_pack_keeps_docs_under_tight_budget() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    for idx in 0..60 {
        std::fs::create_dir_all(root.join("src").join(format!("mod{idx}")))
            .with_context(|| format!("mkdir mod{idx}"))?;
    }
    std::fs::write(root.join("README.md"), "# Hello\n").context("write README.md")?;
    std::fs::create_dir_all(root.join("docs")).context("mkdir docs")?;
    std::fs::write(root.join("docs").join("README.md"), "# Docs\n")
        .context("write docs/README.md")?;

    let args = serde_json::json!({
        "path": root.to_string_lossy(),
        "map_depth": 2,
        "map_limit": 200,
        "docs_limit": 2,
        "doc_max_lines": 20,
        "doc_max_chars": 200,
        "max_chars": 1200
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
        .context("repo_onboarding_pack missing text content")?;
    assert_is_context_doc(text);
    assert!(
        text.contains("N: docs:"),
        "expected docs section marker under tight budget"
    );
    assert!(
        text.contains("R: README.md:1 doc") || text.contains("R: docs/README.md:1 doc"),
        "expected at least one doc slice even under tight budget"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn repo_onboarding_pack_keeps_map_under_tight_budget_even_with_long_docs() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
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

    // Long, noisy doc content should not starve the map section under small budgets.
    // Keep it deterministic (no randomness, no LLM summarization).
    let long_agents = (0..600)
        .map(|i| format!("LINE {i:04} lorem ipsum dolor sit amet, consectetur adipiscing elit.\n"))
        .collect::<String>();
    std::fs::write(root.join("AGENTS.md"), long_agents).context("write AGENTS.md")?;

    let text = call_tool_text(
        &service,
        "repo_onboarding_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "map_depth": 2,
            "map_limit": 50,
            "docs_limit": 2,
            "max_chars": 2000,
            "response_mode": "facts"
        }),
    )
    .await?;

    assert_is_context_doc(&text);
    assert!(text.contains("N: map:"), "expected map section marker");
    assert!(
        text.lines().any(|line| line.trim() == "src"),
        "expected src in map output even with long docs"
    );
    assert!(text.contains("N: docs:"), "expected docs section marker");
    assert!(
        text.contains("R: AGENTS.md:1 doc"),
        "expected AGENTS.md to be surfaced as a doc candidate"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn repo_onboarding_pack_clamps_tiny_budget_and_stays_low_noise_by_default() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
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
        "max_chars": 5
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
        .context("repo_onboarding_pack missing text content")?;

    assert_is_context_doc(text);
    assert!(
        text.starts_with("[CONTENT]\n"),
        "default output should be low-noise `.context` (no legend)"
    );
    assert!(
        text.contains("R: README.md:1 doc"),
        "expected docs to still be present when max_chars is clamped"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
