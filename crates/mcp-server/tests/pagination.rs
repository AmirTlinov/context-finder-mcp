use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use serde_json::Value;
use std::fmt::Write as _;
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

async fn call_tool_json(
    service: &RunningService<RoleClient, ()>,
    name: &str,
    args: serde_json::Value,
) -> Result<Value> {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: name.to_string().into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling tool")??;

    anyhow::ensure!(
        result.is_error != Some(true),
        "tool {name} returned error: {result:?}"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("tool did not return text content")?;
    serde_json::from_str(text).context("tool output is not valid JSON")
}

async fn start_service() -> Result<(tempfile::TempDir, RunningService<RoleClient, ()>)> {
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
    Ok((tmp, service))
}

#[tokio::test]
async fn list_files_supports_cursor_pagination() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(root.join("src").join("a.rs"), "a\n").context("write a.rs")?;
    std::fs::write(root.join("src").join("b.rs"), "b\n").context("write b.rs")?;
    std::fs::write(root.join("src").join("c.rs"), "c\n").context("write c.rs")?;

    assert!(
        !root.join(".context-finder").exists(),
        "temp project unexpectedly has .context-finder before list_files"
    );

    let mut cursor: Option<String> = None;
    let mut seen = Vec::new();
    for _ in 0..5usize {
        let args = serde_json::json!({
            "path": root.to_string_lossy(),
            "file_pattern": "src/*",
            "limit": 1,
            "max_chars": 20_000,
            "cursor": cursor,
        });
        let json = call_tool_json(&service, "list_files", args).await?;
        let files = json
            .get("files")
            .and_then(Value::as_array)
            .context("missing files array")?;
        if files.is_empty() {
            break;
        }
        let first = files
            .first()
            .and_then(Value::as_str)
            .context("file entry is not a string")?
            .to_string();
        seen.push(first);

        cursor = json
            .get("next_cursor")
            .and_then(Value::as_str)
            .map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }

    assert_eq!(
        seen,
        vec![
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "src/c.rs".to_string()
        ]
    );

    assert!(
        !root.join(".context-finder").exists(),
        "list_files created .context-finder side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn map_supports_cursor_pagination() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("a")).context("mkdir a")?;
    std::fs::write(root.join("a").join("main.rs"), "fn a() {}\n").context("write a/main.rs")?;

    std::fs::create_dir_all(root.join("b")).context("mkdir b")?;
    std::fs::write(root.join("b").join("main.rs"), "fn b() {}\n").context("write b/main.rs")?;
    std::fs::write(root.join("b").join("extra.rs"), "fn b2() {}\n").context("write b/extra.rs")?;

    assert!(
        !root.join(".context-finder").exists(),
        "temp project unexpectedly has .context-finder before map"
    );

    let first = call_tool_json(
        &service,
        "map",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "depth": 1,
            "limit": 1,
        }),
    )
    .await?;
    assert_eq!(first.get("truncated").and_then(Value::as_bool), Some(true));
    let first_dir = first
        .get("directories")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|d| d.get("path"))
        .and_then(Value::as_str)
        .context("missing first directory path")?
        .to_string();
    let cursor = first
        .get("next_cursor")
        .and_then(Value::as_str)
        .context("missing next_cursor")?
        .to_string();

    let second = call_tool_json(
        &service,
        "map",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "depth": 1,
            "limit": 1,
            "cursor": cursor,
        }),
    )
    .await?;
    let second_dir = second
        .get("directories")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|d| d.get("path"))
        .and_then(Value::as_str)
        .context("missing second directory path")?
        .to_string();

    assert_ne!(first_dir, second_dir);
    assert_eq!(
        second.get("truncated").and_then(Value::as_bool),
        Some(false)
    );
    assert!(second.get("next_cursor").is_none());

    assert!(
        !root.join(".context-finder").exists(),
        "map created .context-finder side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn text_search_supports_cursor_pagination_filesystem() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("main.rs"),
        "fn main() {\n    println!(\"a\");\n    println!(\"b\");\n    println!(\"c\");\n}\n",
    )
    .context("write main.rs")?;

    assert!(
        !root.join(".context-finder").exists(),
        "temp project unexpectedly has .context-finder before text_search"
    );

    let mut cursor: Option<String> = None;
    let mut lines = Vec::new();
    for _ in 0..5usize {
        let args = serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "println!",
            "file_pattern": "src/*",
            "max_results": 1,
            "case_sensitive": true,
            "whole_word": false,
            "cursor": cursor,
        });
        let json = call_tool_json(&service, "text_search", args).await?;
        let matches = json
            .get("matches")
            .and_then(Value::as_array)
            .context("missing matches array")?;
        if matches.is_empty() {
            break;
        }
        let line = matches
            .first()
            .and_then(|m| m.get("line"))
            .and_then(Value::as_u64)
            .context("missing match line")?;
        lines.push(line);

        cursor = json
            .get("next_cursor")
            .and_then(Value::as_str)
            .map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }

    assert_eq!(lines, vec![2, 3, 4]);

    assert!(
        !root.join(".context-finder").exists(),
        "text_search created .context-finder side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn grep_context_supports_cursor_pagination() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("main.txt"),
        "MATCH\na\nb\nc\nMATCH\nd\ne\n",
    )
    .context("write main.txt")?;

    assert!(
        !root.join(".context-finder").exists(),
        "temp project unexpectedly has .context-finder before grep_context"
    );

    let first = call_tool_json(
        &service,
        "grep_context",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "MATCH",
            "file": "src/main.txt",
            "before": 0,
            "after": 10,
            "max_matches": 100,
            "max_hunks": 100,
            "max_chars": 10,
            "case_sensitive": true,
        }),
    )
    .await?;
    assert_eq!(first.get("truncated").and_then(Value::as_bool), Some(true));
    let cursor = first
        .get("next_cursor")
        .and_then(Value::as_str)
        .context("missing next_cursor")?
        .to_string();

    let second = call_tool_json(
        &service,
        "grep_context",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "MATCH",
            "file": "src/main.txt",
            "before": 0,
            "after": 10,
            "max_matches": 100,
            "max_hunks": 100,
            "max_chars": 10,
            "case_sensitive": true,
            "cursor": cursor,
        }),
    )
    .await?;

    let hunks = second
        .get("hunks")
        .and_then(Value::as_array)
        .context("missing hunks")?;
    anyhow::ensure!(!hunks.is_empty(), "expected hunks in second page");
    let has_second_match = hunks.iter().any(|h| {
        h.get("match_lines")
            .and_then(Value::as_array)
            .is_some_and(|lines| lines.iter().filter_map(Value::as_u64).any(|ln| ln == 5))
    });
    assert!(has_second_match, "expected match line 5 in second page");

    assert!(
        !root.join(".context-finder").exists(),
        "grep_context created .context-finder side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn file_slice_supports_cursor_pagination() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("main.txt"),
        "line1\nline2\nline3\nline4\nline5\n",
    )
    .context("write main.txt")?;

    assert!(
        !root.join(".context-finder").exists(),
        "temp project unexpectedly has .context-finder before file_slice"
    );

    let first = call_tool_json(
        &service,
        "file_slice",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "file": "src/main.txt",
            "start_line": 1,
            "max_lines": 2,
            "max_chars": 20_000,
        }),
    )
    .await?;
    assert_eq!(first.get("start_line").and_then(Value::as_u64), Some(1));
    assert_eq!(first.get("end_line").and_then(Value::as_u64), Some(2));
    assert_eq!(first.get("truncated").and_then(Value::as_bool), Some(true));
    assert_eq!(
        first.get("content").and_then(Value::as_str),
        Some("line1\nline2")
    );
    let cursor = first
        .get("next_cursor")
        .and_then(Value::as_str)
        .context("missing next_cursor")?
        .to_string();

    let second = call_tool_json(
        &service,
        "file_slice",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "file": "src/main.txt",
            "start_line": 999,
            "max_lines": 2,
            "max_chars": 20_000,
            "cursor": cursor,
        }),
    )
    .await?;
    assert_eq!(second.get("start_line").and_then(Value::as_u64), Some(3));
    assert_eq!(second.get("end_line").and_then(Value::as_u64), Some(4));
    assert_eq!(second.get("truncated").and_then(Value::as_bool), Some(true));
    assert_eq!(
        second.get("content").and_then(Value::as_str),
        Some("line3\nline4")
    );
    let cursor2 = second
        .get("next_cursor")
        .and_then(Value::as_str)
        .context("missing next_cursor on second page")?
        .to_string();

    let third = call_tool_json(
        &service,
        "file_slice",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "file": "src/main.txt",
            "max_lines": 2,
            "max_chars": 20_000,
            "cursor": cursor2,
        }),
    )
    .await?;
    assert_eq!(third.get("start_line").and_then(Value::as_u64), Some(5));
    assert_eq!(third.get("end_line").and_then(Value::as_u64), Some(5));
    assert_eq!(third.get("truncated").and_then(Value::as_bool), Some(false));
    assert!(third.get("next_cursor").is_none());
    assert_eq!(third.get("content").and_then(Value::as_str), Some("line5"));

    assert!(
        !root.join(".context-finder").exists(),
        "file_slice created .context-finder side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn read_pack_file_supports_cursor_only_continuation() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("main.txt"),
        "line1\nline2\nline3\nline4\nline5\n",
    )
    .context("write main.txt")?;

    assert!(
        !root.join(".context-finder").exists(),
        "temp project unexpectedly has .context-finder before read_pack"
    );

    let first = call_tool_json(
        &service,
        "read_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "intent": "file",
            "file": "src/main.txt",
            "max_lines": 2,
            "max_chars": 20_000,
        }),
    )
    .await?;
    assert_eq!(first.get("intent").and_then(Value::as_str), Some("file"));

    let sections = first
        .get("sections")
        .and_then(Value::as_array)
        .context("missing sections array")?;
    anyhow::ensure!(!sections.is_empty(), "expected non-empty sections");

    let first_section = sections.first().context("missing first section")?;
    assert_eq!(
        first_section.get("type").and_then(Value::as_str),
        Some("file_slice")
    );
    let first_slice = first_section
        .get("result")
        .context("missing file_slice result")?;
    assert_eq!(
        first_slice.get("start_line").and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(first_slice.get("end_line").and_then(Value::as_u64), Some(2));
    let cursor = first_slice
        .get("next_cursor")
        .and_then(Value::as_str)
        .context("missing file_slice next_cursor")?
        .to_string();

    let second = call_tool_json(
        &service,
        "read_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "cursor": cursor,
        }),
    )
    .await?;
    assert_eq!(second.get("intent").and_then(Value::as_str), Some("file"));

    let sections = second
        .get("sections")
        .and_then(Value::as_array)
        .context("missing sections array")?;
    let second_section = sections.first().context("missing first section")?;
    assert_eq!(
        second_section.get("type").and_then(Value::as_str),
        Some("file_slice")
    );
    let second_slice = second_section
        .get("result")
        .context("missing file_slice result")?;
    assert_eq!(
        second_slice.get("start_line").and_then(Value::as_u64),
        Some(3)
    );

    assert!(
        !root.join(".context-finder").exists(),
        "read_pack created .context-finder side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn read_pack_grep_supports_cursor_only_continuation() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;

    let mut content = String::new();
    for idx in 1..=2000usize {
        writeln!(&mut content, "MATCH {idx}").expect("write line");
    }
    std::fs::write(root.join("src").join("main.txt"), content).context("write main.txt")?;

    assert!(
        !root.join(".context-finder").exists(),
        "temp project unexpectedly has .context-finder before read_pack"
    );

    let first = call_tool_json(
        &service,
        "read_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "intent": "grep",
            "pattern": "MATCH",
            "file": "src/main.txt",
            "before": 0,
            "after": 0,
            "max_chars": 8000,
        }),
    )
    .await?;
    assert_eq!(first.get("intent").and_then(Value::as_str), Some("grep"));

    let first_section = first
        .get("sections")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .with_context(|| format!("missing first section; got: {first:?}"))?;
    assert_eq!(
        first_section.get("type").and_then(Value::as_str),
        Some("grep_context")
    );
    let first_grep = first_section.get("result").context("missing grep result")?;
    assert_eq!(
        first_grep.get("truncated").and_then(Value::as_bool),
        Some(true)
    );
    let cursor = first_grep
        .get("next_cursor")
        .and_then(Value::as_str)
        .context("missing grep next_cursor")?
        .to_string();

    let max_line_first = first_grep
        .get("hunks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|h| {
            h.get("match_lines")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(Value::as_u64)
        .max()
        .unwrap_or(0);
    anyhow::ensure!(max_line_first > 0, "expected match_lines in first page");

    let second = call_tool_json(
        &service,
        "read_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "cursor": cursor,
        }),
    )
    .await?;
    assert_eq!(second.get("intent").and_then(Value::as_str), Some("grep"));

    let second_section = second
        .get("sections")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .context("missing second section")?;
    assert_eq!(
        second_section.get("type").and_then(Value::as_str),
        Some("grep_context")
    );
    let second_grep = second_section
        .get("result")
        .context("missing grep result")?;
    let max_line_second = second_grep
        .get("hunks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|h| {
            h.get("match_lines")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(Value::as_u64)
        .max()
        .unwrap_or(0);
    anyhow::ensure!(
        max_line_second > max_line_first,
        "expected cursor to advance match lines"
    );

    assert!(
        !root.join(".context-finder").exists(),
        "read_pack created .context-finder side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn read_pack_clamps_tiny_budget_and_keeps_continuation() -> Result<()> {
    let (tmp, service) = start_service().await?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    let mut content = String::new();
    for idx in 1..=400usize {
        writeln!(&mut content, "line {idx}").expect("write line");
    }
    std::fs::write(root.join("src").join("main.txt"), content).context("write main.txt")?;

    let json = call_tool_json(
        &service,
        "read_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "intent": "file",
            "file": "src/main.txt",
            "max_lines": 400,
            "max_chars": 1,
        }),
    )
    .await?;

    let budget = json.get("budget").context("missing budget")?;
    let max_chars = budget
        .get("max_chars")
        .and_then(Value::as_u64)
        .context("budget.max_chars missing")?;
    assert!(max_chars >= 1000, "expected min budget clamp");
    assert_eq!(
        budget.get("truncated").and_then(Value::as_bool),
        Some(true),
        "expected truncated response"
    );

    let next_actions = json
        .get("next_actions")
        .and_then(Value::as_array)
        .context("missing next_actions")?;
    assert!(!next_actions.is_empty(), "expected continuation action");

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
