use anyhow::{Context, Result};
use context_vector_store::context_dir_for_project_root;
use rmcp::{
    model::{CallToolRequestParam, CallToolResult},
    service::ServiceExt,
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

fn tool_text(result: &CallToolResult) -> Result<&str> {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("tool did not return text output")
}

#[tokio::test]
async fn rg_works_without_index_and_merges_ranges() -> Result<()> {
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

    let mut lines = Vec::new();
    for i in 1..=12usize {
        if i == 5 || i == 7 {
            lines.push(format!("line {i}: TARGET"));
        } else {
            lines.push(format!("line {i}: filler"));
        }
    }
    std::fs::write(root.join("src").join("a.txt"), lines.join("\n") + "\n")
        .context("write a.txt")?;
    std::fs::write(
        root.join("src").join("b.txt"),
        "one\nTwo\nthree TARGET\nfour\n",
    )
    .context("write b.txt")?;

    let context_dir = context_dir_for_project_root(root);
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "temp project unexpectedly has a context dir before rg"
    );

    let args = serde_json::json!({
        "path": root.to_string_lossy(),
        "pattern": "TARGET",
        "file_pattern": "src/*",
        "before": 2,
        "after": 2,
        "max_matches": 100,
        "max_hunks": 10,
        "max_chars": 20_000,
        "case_sensitive": true,
        "response_mode": "full",
    });
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "rg".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling rg")??;

    assert_ne!(result.is_error, Some(true), "rg returned error");
    let text = tool_text(&result)?;
    assert!(
        text.contains("pattern=TARGET"),
        "expected rg summary to include pattern=TARGET"
    );
    assert!(
        text.contains("R: src/a.txt:3"),
        "expected merged hunk to start at line 3"
    );
    assert!(text.contains("line 5: TARGET"));
    assert!(text.contains("line 7: TARGET"));
    assert!(
        text.contains("line 9: filler"),
        "expected merged range to include line 9"
    );

    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "rg created project context side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn rg_can_be_case_insensitive_and_reports_max_chars_truncation() -> Result<()> {
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
    let long_tail = "c".repeat(20_000);
    std::fs::write(
        root.join("src").join("main.txt"),
        format!("aaa\nTARGETTARGETTARGETTARGET\n{long_tail}\n"),
    )
    .context("write main.txt")?;

    let context_dir = context_dir_for_project_root(root);
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "temp project unexpectedly has a context dir before rg"
    );

    let args = serde_json::json!({
        "path": root.to_string_lossy(),
        "pattern": "target",
        "file": "src/main.txt",
        "before": 1,
        "after": 1,
        "max_matches": 10,
        "max_hunks": 10,
        "max_chars": 8000,
        "case_sensitive": false,
        "response_mode": "full",
    });
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "rg".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling rg")??;

    assert_ne!(result.is_error, Some(true), "rg returned error");
    let text = tool_text(&result)?;
    assert!(
        text.contains("TARGETTARGETTARGETTARGET"),
        "expected match line in output"
    );
    assert!(text.contains("\nM: "), "expected truncation cursor (M:)");
    assert!(
        !text.contains(&long_tail),
        "expected long tail to be truncated"
    );

    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "rg created project context side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn rg_budget_trimming_keeps_match_line_visible() -> Result<()> {
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

    // Place the match far from the start so naive "keep first half" trimming would eventually
    // drop it and return only prelude lines (bad UX). The tool should keep at least one match
    // line visible even under tight max_chars budgets.
    let mut lines = Vec::new();
    for i in 1..=220usize {
        if i == 150 {
            lines.push(format!("line {i}: TARGET"));
        } else {
            lines.push(format!("line {i}: filler filler filler filler filler"));
        }
    }
    std::fs::write(root.join("src").join("main.txt"), lines.join("\n") + "\n")
        .context("write main.txt")?;

    let args = serde_json::json!( {
        "path": root.to_string_lossy(),
        "pattern": "TARGET",
        "file": "src/main.txt",
        "before": 120,
        "after": 120,
        "max_hunks": 1,
        "max_chars": 800,
        "format": "plain",
        "response_mode": "minimal",
    });
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "rg".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling rg")??;

    assert_ne!(result.is_error, Some(true), "rg returned error");
    let text = tool_text(&result)?;
    assert!(
        text.contains("line 150: TARGET"),
        "expected TARGET match line to remain visible under trimming"
    );
    assert!(text.contains("\nM: "), "expected truncation cursor (M:)");

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn rg_minimal_small_budget_still_returns_payload_hunks() -> Result<()> {
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

    let mut content = String::new();
    let noisy = "\\\"\\\\\\\"".repeat(18);
    content.push_str(&format!("TARGET {noisy} {}\n", "x".repeat(40)));
    for i in 2..=200usize {
        content.push_str(&format!("line {i} {noisy} {}\n", "y".repeat(40)));
    }
    std::fs::write(root.join("src").join("main.txt"), content).context("write main.txt")?;

    let args = serde_json::json!({
        "path": root.to_string_lossy(),
        "pattern": "TARGET",
        "file": "src/main.txt",
        "before": 0,
        "after": 5000,
        "max_matches": 10_000,
        "max_hunks": 10,
        "max_chars": 1500,
        "case_sensitive": true,
        "response_mode": "minimal",
    });
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "rg".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling rg")??;

    assert_ne!(result.is_error, Some(true), "rg returned error");
    let text = tool_text(&result)?;
    assert!(
        text.contains("pattern=TARGET"),
        "expected rg summary to include pattern=TARGET"
    );
    assert!(text.contains("TARGET"), "expected match text in output");
    assert!(text.contains("\nM: "), "expected truncation cursor (M:)");

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn rg_supports_literal_mode_and_cursor_only_continuation() -> Result<()> {
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
    std::fs::write(root.join("src").join("main.txt"), "a+b\nab\naaab\na+b\n")
        .context("write main.txt")?;

    let first_args = serde_json::json!({
        "path": root.to_string_lossy(),
        "pattern": "a+b",
        "literal": true,
        "file": "src/main.txt",
        "before": 0,
        "after": 0,
        "max_matches": 100,
        "max_hunks": 1,
        "max_chars": 20_000,
        "case_sensitive": true,
        "response_mode": "full",
    });
    let first = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "rg".into(),
            arguments: first_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling rg")??;

    assert_ne!(first.is_error, Some(true), "rg returned error");
    let first_text = tool_text(&first)?;
    assert!(first_text.contains("1:* a+b"));
    let cursor = first_text
        .lines()
        .find_map(|line| {
            let cursor = line.strip_prefix("M: ")?;
            (cursor.starts_with("cfcs2:") || cursor.starts_with("cfcs1:"))
                .then(|| cursor.to_string())
        })
        .context("missing cursor (M:) line")?;

    let second = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "rg".into(),
            arguments: serde_json::json!({
                "path": root.to_string_lossy(),
                "cursor": cursor,
            })
            .as_object()
            .cloned(),
        }),
    )
    .await
    .context("timeout calling rg (cursor-only)")??;
    assert_ne!(
        second.is_error,
        Some(true),
        "rg returned error (cursor-only)"
    );
    let second_text = tool_text(&second)?;
    assert!(
        !second_text.contains("\nM: "),
        "did not expect a cursor line in cursor-only continuation response"
    );
    assert!(second_text.contains("4:* a+b"));

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn rg_tight_budget_max_chars_truncation_still_returns_cursor() -> Result<()> {
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

    // Create many small hunks with long per-hunk metadata (file paths).
    // This can overflow the JSON `max_chars` budget even if the total hunk `content` stays small.
    for i in 0..40usize {
        let file = format!(
            "file_{i:04}_this_is_a_very_long_file_name_used_to_stress_json_envelope_overhead.txt"
        );
        std::fs::write(root.join("src").join(file), "TARGET\n").context("write src file")?;
    }

    let args = serde_json::json!({
        "path": root.to_string_lossy(),
        "pattern": "TARGET",
        "file_pattern": "src/*",
        "before": 0,
        "after": 0,
        "max_matches": 10_000,
        "max_hunks": 10_000,
        "max_chars": 800,
        "case_sensitive": true,
        "response_mode": "minimal",
    });
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "rg".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling rg")??;

    assert_ne!(result.is_error, Some(true), "rg returned error");
    assert!(
        tool_text(&result)?.contains("TARGET"),
        "expected at least one match under tight budget"
    );
    assert!(
        tool_text(&result)?.contains("\nM: "),
        "expected truncation cursor (M:)"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
