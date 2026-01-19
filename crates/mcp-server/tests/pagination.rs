use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use std::collections::BTreeSet;
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

async fn start_service() -> Result<(tempfile::TempDir, RunningService<RoleClient, ()>)> {
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
    Ok((tmp, service))
}

fn tool_text(result: &rmcp::model::CallToolResult) -> Result<&str> {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("tool did not return text output")
}

fn extract_cursor(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let cursor = line.strip_prefix("M: ")?;
        if cursor.chars().any(char::is_whitespace) {
            return None;
        }
        Some(cursor.to_string())
    })
}

fn extract_bare_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            if trimmed.starts_with('[')
                || trimmed.starts_with("A:")
                || trimmed.starts_with("N:")
                || trimmed.starts_with("R:")
                || trimmed.starts_with("M:")
            {
                return None;
            }
            Some(trimmed.to_string())
        })
        .collect()
}

async fn call_tool(
    service: &RunningService<RoleClient, ()>,
    name: &str,
    args: serde_json::Value,
) -> Result<rmcp::model::CallToolResult> {
    tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: name.to_string().into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling tool")?
    .context("call tool")
}

#[tokio::test]
async fn list_files_supports_cursor_pagination() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(root.join("src").join("a.rs"), "a\n").context("write a.rs")?;
    std::fs::write(root.join("src").join("b.rs"), "b\n").context("write b.rs")?;
    std::fs::write(root.join("src").join("c.rs"), "c\n").context("write c.rs")?;

    let mut cursor: Option<String> = None;
    let mut seen = BTreeSet::new();
    for _ in 0..8usize {
        // Cursor continuations should be robust to changing limit/max_chars: they affect paging
        // shape and budget, but not the semantic continuation token.
        let (limit, max_chars) = if cursor.is_some() {
            (2, 1_500)
        } else {
            (1, 2_000)
        };
        let result = call_tool(
            &service,
            "list_files",
            serde_json::json!({
                "path": root.to_string_lossy(),
                "file_pattern": "src/*",
                "limit": limit,
                "max_chars": max_chars,
                "cursor": cursor,
                "response_mode": "minimal",
            }),
        )
        .await?;
        assert_ne!(result.is_error, Some(true), "list_files returned error");

        let text = tool_text(&result)?;
        let files = extract_bare_lines(text);
        for file in files {
            seen.insert(file);
        }
        cursor = extract_cursor(text);
        if cursor.is_none() {
            break;
        }
    }

    let expected: BTreeSet<String> = ["src/a.rs", "src/b.rs", "src/c.rs"]
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(seen, expected);

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn file_slice_supports_cursor_pagination() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::write(root.join("README.md"), "one\ntwo\nthree\n").context("write README.md")?;

    let mut cursor: Option<String> = None;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for _ in 0..4usize {
        // Cursor continuations should be robust to changing max_lines: it affects paging shape,
        // but not the semantic continuation token.
        let max_lines = if cursor.is_some() { 2 } else { 1 };
        let result = call_tool(
            &service,
            "file_slice",
            serde_json::json!({
                "path": root.to_string_lossy(),
                "file": "README.md",
                "max_lines": max_lines,
                "max_chars": 1024,
                "cursor": cursor,
                "response_mode": "minimal",
            }),
        )
        .await?;
        assert_ne!(result.is_error, Some(true), "file_slice returned error");

        let text = tool_text(&result)?;
        for line in extract_bare_lines(text) {
            if ["one", "two", "three"].contains(&line.as_str()) {
                seen.insert(line);
            }
        }

        cursor = extract_cursor(text);
        if cursor.is_none() {
            break;
        }
    }

    let expected: BTreeSet<String> = ["one", "two", "three"]
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(seen, expected);

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn grep_context_supports_cursor_pagination() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(root.join("src").join("main.txt"), "a+b\nab\naaab\na+b\n")
        .context("write main.txt")?;

    let first = call_tool(
        &service,
        "grep_context",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "a+b",
            "literal": true,
            "file": "src/main.txt",
            "before": 0,
            "after": 0,
            "max_hunks": 1,
            "max_chars": 2000,
            "response_mode": "full",
        }),
    )
    .await?;
    assert_ne!(first.is_error, Some(true), "grep_context returned error");
    let first_text = tool_text(&first)?;
    assert!(first_text.contains("1:* a+b"));
    let cursor = extract_cursor(first_text).context("missing cursor (M:) in grep_context")?;

    let second = call_tool(
        &service,
        "grep_context",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "cursor": cursor,
        }),
    )
    .await?;
    assert_ne!(
        second.is_error,
        Some(true),
        "grep_context returned error (cursor-only)"
    );
    let second_text = tool_text(&second)?;
    assert!(second_text.contains("4:* a+b"));

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn text_search_supports_cursor_pagination() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("main.txt"),
        "needle\nfiller\nneedle\nfiller\nneedle\n",
    )
    .context("write main.txt")?;

    let first = call_tool(
        &service,
        "text_search",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "needle",
            "max_results": 1,
            "max_chars": 1200,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(first.is_error, Some(true), "text_search returned error");
    let first_text = tool_text(&first)?;
    assert!(first_text.contains("needle"));
    let cursor = extract_cursor(first_text).context("missing cursor (M:) in text_search")?;

    let second = call_tool(
        &service,
        "text_search",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "cursor": cursor,
            "max_chars": 1200,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(
        second.is_error,
        Some(true),
        "text_search returned error (cursor-only)"
    );
    let second_text = tool_text(&second)?;
    assert!(second_text.contains("needle"));

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn read_pack_supports_cursor_continuation() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    let mut big = String::new();
    for idx in 1..=160usize {
        big.push_str(&format!("line {idx}: {}\n", "x".repeat(40)));
    }
    std::fs::write(root.join("BIG.md"), big).context("write BIG.md")?;

    let first = call_tool(
        &service,
        "read_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "file": "BIG.md",
            "start_line": 1,
            "max_lines": 50,
            "max_chars": 300,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(first.is_error, Some(true), "read_pack returned error");
    let first_text = tool_text(&first)?;
    assert!(first_text.contains("[CONTENT]"));
    let cursor = extract_cursor(first_text).context("missing cursor (M:) in read_pack")?;

    let second = call_tool(
        &service,
        "read_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "cursor": cursor,
            "max_lines": 60,
            "max_chars": 300,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        second.is_error,
        Some(true),
        "read_pack returned error (cursor)"
    );
    let second_text = tool_text(&second)?;
    assert!(second_text.contains("[CONTENT]"));

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn map_supports_cursor_continuation_with_limit_change() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::create_dir_all(root.join("docs")).context("mkdir docs")?;
    std::fs::write(root.join("src").join("a.rs"), "a\n").context("write a.rs")?;
    std::fs::write(root.join("src").join("b.rs"), "b\n").context("write b.rs")?;
    std::fs::write(root.join("docs").join("c.md"), "c\n").context("write c.md")?;

    let first = call_tool(
        &service,
        "map",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "depth": 2,
            "limit": 1,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(first.is_error, Some(true), "map returned error");
    let first_text = tool_text(&first)?;
    let cursor = extract_cursor(first_text).context("missing cursor (M:) in map")?;

    let second = call_tool(
        &service,
        "map",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "cursor": cursor,
            "depth": 2,
            "limit": 2,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(
        second.is_error,
        Some(true),
        "map returned error (cursor with changed limit)"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
