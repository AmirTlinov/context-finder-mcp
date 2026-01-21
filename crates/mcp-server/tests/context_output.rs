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

fn assert_is_low_noise_context_doc(text: &str) {
    assert_is_context_doc(text);
    assert!(
        text.starts_with("[CONTENT]\n"),
        "low-noise `.context` output should start with [CONTENT] (no empty [LEGEND] block)"
    );
}

#[tokio::test]
async fn read_tools_return_low_noise_context_docs() -> Result<()> {
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
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("lib.rs"),
        "pub fn alpha() {}\n\npub fn beta() { alpha(); }\n",
    )
    .context("write src/lib.rs")?;
    std::fs::write(
        root.join("README.md"),
        "# Demo\n\nThis repo exists for testing.\n",
    )
    .context("write README.md")?;

    let cat = call_tool_text(
        &service,
        "cat",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "file": "src/lib.rs",
            "max_lines": 200,
            "max_chars": 400,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_is_low_noise_context_doc(&cat);

    let rg = call_tool_text(
        &service,
        "rg",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "alpha",
            "max_chars": 400,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_is_low_noise_context_doc(&rg);

    let text_search = call_tool_text(
        &service,
        "text_search",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "alpha",
            "max_chars": 600,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_is_low_noise_context_doc(&text_search);

    let read_pack = call_tool_text(
        &service,
        "read_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "max_chars": 900,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_is_low_noise_context_doc(&read_pack);

    Ok(())
}

#[tokio::test]
async fn default_output_is_low_noise_context() -> Result<()> {
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
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(root.join("src").join("lib.rs"), "pub fn alpha() {}\n")
        .context("write src/lib.rs")?;

    let cat = call_tool_text(
        &service,
        "cat",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "file": "src/lib.rs",
            "max_lines": 50,
            "max_chars": 300,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_is_low_noise_context_doc(&cat);

    Ok(())
}

#[tokio::test]
async fn text_search_context_output_uses_budget_effectively() -> Result<()> {
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
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("main.rs"),
        "needle a\nneedle b\nneedle c\nneedle d\nneedle e\n",
    )
    .context("write src/main.rs")?;

    let max_chars = 300usize;
    let text = call_tool_text(
        &service,
        "text_search",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "needle",
            "file_pattern": "src/*",
            "max_chars": max_chars,
            "response_mode": "minimal",
        }),
    )
    .await?;

    assert_is_low_noise_context_doc(&text);
    anyhow::ensure!(
        text.chars().count() <= max_chars,
        "expected context output to respect max_chars (used_chars={}, max_chars={})",
        text.chars().count(),
        max_chars
    );

    // With `.context` grouping, a small budget should still fit all matches without triggering a cursor.
    assert!(!text.contains("\nM:"), "did not expect pagination cursor");
    for expected in [
        "1:1: needle a",
        "2:1: needle b",
        "3:1: needle c",
        "4:1: needle d",
        "5:1: needle e",
    ] {
        assert!(
            text.contains(expected),
            "expected text_search output to contain match line: {expected}"
        );
    }

    Ok(())
}

#[tokio::test]
async fn cat_context_output_respects_max_chars() -> Result<()> {
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
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("lib.rs"),
        "pub fn alpha() {}\n\npub fn beta() { alpha(); }\n",
    )
    .context("write src/lib.rs")?;

    let max_chars = 260usize;
    let text = call_tool_text(
        &service,
        "cat",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "file": "src/lib.rs",
            "max_lines": 50,
            "max_chars": max_chars,
            "response_mode": "minimal",
        }),
    )
    .await?;

    assert_is_low_noise_context_doc(&text);
    anyhow::ensure!(
        text.chars().count() <= max_chars,
        "expected cat context output to respect max_chars (used_chars={}, max_chars={})",
        text.chars().count(),
        max_chars
    );
    assert!(
        text.contains("pub fn alpha()"),
        "expected file content in response"
    );

    Ok(())
}

#[tokio::test]
async fn cat_handles_long_paths_under_tight_budgets() -> Result<()> {
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
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;

    // Long relative file path + lots of content is the worst case for envelope budgeting.
    let long_name = format!("{}.rs", "a".repeat(180));
    let rel = format!("src/{long_name}");
    let content = "let v = 1; // filler\n".repeat(500);
    std::fs::write(root.join(&rel), content).context("write long file")?;

    let max_chars = 2_000usize;
    let text = call_tool_text(
        &service,
        "cat",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "file": rel,
            "max_lines": 2000,
            "max_chars": max_chars,
            "response_mode": "minimal",
        }),
    )
    .await?;

    assert_is_low_noise_context_doc(&text);
    anyhow::ensure!(
        text.chars().count() <= max_chars,
        "expected long-path cat to respect max_chars (used_chars={}, max_chars={})",
        text.chars().count(),
        max_chars
    );
    assert!(
        text.contains("let v = 1;"),
        "expected cat to include file content under long path"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn ls_context_output_respects_max_chars_and_keeps_cursor() -> Result<()> {
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
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;

    for idx in 0..80usize {
        let name = format!("src/{}_{}.rs", "very_long_file_name".repeat(3), idx);
        std::fs::write(root.join(&name), format!("// {idx}\n")).context("write file")?;
    }

    let max_chars = 260usize;
    let text = call_tool_text(
        &service,
        "ls",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "file_pattern": "src/*",
            "max_chars": max_chars,
            "response_mode": "minimal",
        }),
    )
    .await?;

    assert_is_low_noise_context_doc(&text);
    anyhow::ensure!(
        text.chars().count() <= max_chars,
        "expected ls `.context` output to respect max_chars (used_chars={}, max_chars={})",
        text.chars().count(),
        max_chars
    );
    assert!(
        text.contains("\nM: "),
        "expected ls to include a cursor line (M:) under tight budgets"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
