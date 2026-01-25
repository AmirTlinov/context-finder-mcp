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

#[tokio::test]
async fn text_search_works_without_index_and_is_bounded() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

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

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("main.rs"),
        "fn main() {\n    println!(\"Hello\");\n}\n",
    )
    .context("write main.rs")?;

    let context_dir = context_dir_for_project_root(root);
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "temp project unexpectedly has a context dir before text_search"
    );

    let args = serde_json::json!({
        "path": root.to_string_lossy(),
        "pattern": "println!",
        "max_results": 5,
        "case_sensitive": true,
        "whole_word": false,
    });
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "text_search".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling text_search")??;

    assert_ne!(result.is_error, Some(true), "text_search returned error");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("text_search missing text output")?;
    assert!(text.contains("[CONTENT]"));
    assert!(text.contains("A: Matches:"), "missing answer line");
    assert!(
        text.contains("pattern=println!"),
        "missing pattern in answer"
    );
    assert!(
        text.contains("-- src/main.rs --"),
        "expected src/main.rs match in output"
    );

    // Must not create indexes/corpus as a side effect.
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "text_search created project context side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn text_search_respects_max_chars_and_supports_cursor_only_continuation() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

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

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;

    // Many long match lines -> forces max_chars truncation in a deterministic way.
    let long_tail = "x".repeat(400);
    let mut content = String::new();
    for idx in 0..20usize {
        content.push_str(&format!("// {idx}\n"));
        content.push_str(&format!("let v{idx} = \"render {long_tail}\";\n"));
    }
    std::fs::write(root.join("src").join("main.rs"), content).context("write main.rs")?;

    let context_dir = context_dir_for_project_root(root);
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "temp project unexpectedly has a context dir before text_search"
    );

    let max_chars = 320usize;
    let args = serde_json::json!({
        "path": root.to_string_lossy(),
        "pattern": "render",
        "file_pattern": "src/*",
        "max_results": 100,
        "max_chars": max_chars,
        "case_sensitive": true,
        "whole_word": false,
        "response_mode": "minimal",
    });
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "text_search".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling text_search")??;

    assert_ne!(result.is_error, Some(true), "text_search returned error");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("text_search did not return text content")?;
    anyhow::ensure!(
        text.chars().count() <= max_chars,
        "expected text_search to respect max_chars (used_chars={}, max_chars={})",
        text.chars().count(),
        max_chars
    );

    let cursor = text
        .lines()
        .find_map(|line| line.strip_prefix("M: ").map(str::to_string))
        .context("expected cursor line (M:) when truncated")?;
    anyhow::ensure!(
        cursor.len() < 200,
        "expected compact cursor alias, got len={}",
        cursor.len()
    );

    // Cursor-only continuation: pattern and other options are inferred from the cursor.
    let args2 = serde_json::json!({
        "path": root.to_string_lossy(),
        "cursor": cursor,
        "max_chars": max_chars,
        "response_mode": "minimal",
    });
    let result2 = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "text_search".into(),
            arguments: args2.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling text_search continuation")??;

    assert_ne!(
        result2.is_error,
        Some(true),
        "text_search continuation returned error"
    );
    let text2 = result2
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("text_search continuation did not return text content")?;
    anyhow::ensure!(
        text2.chars().count() <= max_chars,
        "expected continuation to respect max_chars"
    );
    anyhow::ensure!(
        text2.contains("-- src/main.rs --"),
        "expected continuation to return matches"
    );

    // Must not create indexes/corpus as a side effect.
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "text_search created project context side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
