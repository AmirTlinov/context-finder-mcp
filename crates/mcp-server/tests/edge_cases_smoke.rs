use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
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

async fn start_service(cwd: &Path) -> Result<RunningService<RoleClient, ()>> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.current_dir(cwd);
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

async fn call_tool(
    service: &RunningService<RoleClient, ()>,
    name: &str,
    args: Value,
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
    .context("tool call failed")
}

fn tool_text(result: &rmcp::model::CallToolResult) -> Result<&str> {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("missing text content")
}

fn extract_cursor(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("M: ").map(str::trim))
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn assert_error_code(result: &rmcp::model::CallToolResult, expected: &str) -> Result<()> {
    assert_eq!(result.is_error, Some(true));
    let text = tool_text(result)?;
    assert!(
        text.contains(&format!("A: error: {expected}")),
        "expected error code {expected}, got:\n{text}"
    );
    Ok(())
}

#[tokio::test]
async fn edge_cases_smoke_pack_is_low_noise_and_fail_closed() -> Result<()> {
    let root1 = tempfile::tempdir().context("tempdir root1")?;
    let root1_path = root1.path();

    std::fs::create_dir_all(root1_path.join("src")).context("mkdir src")?;
    std::fs::write(
        root1_path.join("src").join("main.rs"),
        "fn main() {\n  println!(\"hi\");\n}\n",
    )
    .context("write src/main.rs")?;
    std::fs::write(root1_path.join("README.md"), "one\ntwo\nthree\n").context("write README.md")?;
    std::fs::write(root1_path.join(".env"), "SECRET=1\n").context("write .env")?;

    // Outside-root file for traversal checks.
    let outside = root1_path
        .parent()
        .context("root1 has no parent")?
        .join("outside.txt");
    std::fs::write(&outside, "OUTSIDE\n").context("write outside.txt")?;

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, root1_path.join("link_out"))
            .context("symlink link_out")?;
    }

    let service = start_service(root1_path).await?;

    // 1) Default root inference: first call without `path` should work and not leak secrets.
    let ls_default = call_tool(
        &service,
        "ls",
        serde_json::json!({
            "limit": 50,
            "max_chars": 4000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(ls_default.is_error, Some(true), "ls should succeed");
    let ls_text = tool_text(&ls_default)?;
    assert!(
        ls_text.contains("src/main.rs"),
        "expected src/main.rs in ls"
    );
    assert!(
        !ls_text.contains(".env"),
        "did not expect .env in ls by default"
    );

    // 2) `cat` basic read (no explicit path).
    let cat_main = call_tool(
        &service,
        "cat",
        serde_json::json!({
            "file": "src/main.rs",
            "max_lines": 20,
            "max_chars": 4000,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(cat_main.is_error, Some(true), "cat should succeed");
    let cat_text = tool_text(&cat_main)?;
    assert!(
        cat_text.contains("R: src/main.rs:1"),
        "expected file ref in cat"
    );
    assert!(
        cat_text.contains("fn main()"),
        "expected file content in cat"
    );

    // 3) response_mode compat: `compact` should deserialize (alias for minimal).
    let cat_compact = call_tool(
        &service,
        "cat",
        serde_json::json!({
            "file": "README.md",
            "max_lines": 1,
            "max_chars": 2000,
            "response_mode": "compact",
        }),
    )
    .await?;
    assert_ne!(
        cat_compact.is_error,
        Some(true),
        "cat compact should succeed"
    );

    // 4) Fail-closed: secrets are refused by default.
    let cat_env_denied = call_tool(
        &service,
        "cat",
        serde_json::json!({
            "file": ".env",
            "max_lines": 5,
            "max_chars": 2000,
        }),
    )
    .await?;
    assert_error_code(&cat_env_denied, "invalid_request")?;

    let cat_env_allowed = call_tool(
        &service,
        "cat",
        serde_json::json!({
            "file": ".env",
            "max_lines": 5,
            "max_chars": 2000,
            "allow_secrets": true,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(
        cat_env_allowed.is_error,
        Some(true),
        "cat allow_secrets should succeed"
    );
    assert!(tool_text(&cat_env_allowed)?.contains("SECRET=1"));

    // 5) Fail-closed: path traversal (outside-root file).
    let cat_traversal = call_tool(
        &service,
        "cat",
        serde_json::json!({
            "file": "../outside.txt",
            "max_lines": 5,
            "max_chars": 2000,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_error_code(&cat_traversal, "invalid_request")?;
    assert!(
        tool_text(&cat_traversal)?.contains("outside project root"),
        "expected outside-root refusal"
    );

    // 6) Defensive: file paths with embedded control chars should be rejected.
    let cat_control = call_tool(
        &service,
        "cat",
        serde_json::json!({
            "file": "src/\nmain.rs",
            "max_lines": 5,
            "max_chars": 2000,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_error_code(&cat_control, "invalid_request")?;
    assert!(
        tool_text(&cat_control)?.contains("control characters"),
        "expected control-char hint"
    );

    // 7) Symlink escapes should be refused (unix-only).
    #[cfg(unix)]
    {
        let cat_symlink = call_tool(
            &service,
            "cat",
            serde_json::json!({
                "file": "link_out",
                "max_lines": 5,
                "max_chars": 2000,
                "response_mode": "minimal",
            }),
        )
        .await?;
        assert_error_code(&cat_symlink, "invalid_request")?;
        assert!(
            tool_text(&cat_symlink)?.contains("outside project root"),
            "expected symlink escape refusal"
        );
    }

    // 8) `tree` should also hide secrets.
    let tree = call_tool(
        &service,
        "tree",
        serde_json::json!({
            "depth": 3,
            "limit": 100,
            "max_chars": 4000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(tree.is_error, Some(true), "tree should succeed");
    let tree_text = tool_text(&tree)?;
    assert!(tree_text.contains("tree:"), "expected tree output header");
    assert!(tree_text.contains("src"), "expected src in tree");
    assert!(!tree_text.contains(".env"), "did not expect .env in tree");

    // 9) `rg` invalid regex should be invalid_request.
    let rg_invalid = call_tool(
        &service,
        "rg",
        serde_json::json!({
            "pattern": "(",
            "literal": false,
            "max_hunks": 5,
            "max_chars": 4000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_error_code(&rg_invalid, "invalid_request")?;

    // 10) `rg` literal mode should accept the same pattern and return results.
    let rg_literal = call_tool(
        &service,
        "rg",
        serde_json::json!({
            "pattern": "(",
            "literal": true,
            "max_hunks": 1,
            "max_chars": 2000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(rg_literal.is_error, Some(true), "rg literal should succeed");
    assert!(
        tool_text(&rg_literal)?.contains("src/main.rs"),
        "expected src/main.rs in rg output"
    );
    let rg_cursor = extract_cursor(tool_text(&rg_literal)?);
    if let Some(cursor) = rg_cursor {
        let rg_cursor_only = call_tool(&service, "rg", serde_json::json!({ "cursor": cursor }))
            .await
            .context("rg cursor-only call failed")?;
        assert_ne!(
            rg_cursor_only.is_error,
            Some(true),
            "rg cursor-only should succeed"
        );
    }

    // 11) `read_pack` file intent: missing file should be invalid_request (not internal).
    let read_missing = call_tool(
        &service,
        "read_pack",
        serde_json::json!({
            "intent": "file",
            "file": "does_not_exist.txt",
            "max_lines": 20,
            "max_chars": 4000,
        }),
    )
    .await?;
    assert_error_code(&read_missing, "invalid_request")?;

    // 12) `read_pack` should reject embedded control chars in file paths.
    let read_control = call_tool(
        &service,
        "read_pack",
        serde_json::json!({
            "intent": "file",
            "file": "src/\nmain.rs",
            "max_lines": 20,
            "max_chars": 4000,
        }),
    )
    .await?;
    assert_error_code(&read_control, "invalid_request")?;
    assert!(
        tool_text(&read_control)?.contains("control characters"),
        "expected control-char hint in read_pack"
    );

    // 13) `meaning_pack` should work on tiny repos and be bounded/low-noise.
    let meaning = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "query": "how to run tests",
            "max_chars": 6000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(meaning.is_error, Some(true), "meaning_pack should succeed");
    assert!(
        tool_text(&meaning)?.contains("A: meaning_pack"),
        "expected meaning_pack answer line"
    );

    // 14) `meaning_focus` should accept a focus target and stay bounded.
    let focus = call_tool(
        &service,
        "meaning_focus",
        serde_json::json!({
            "focus": "src/main.rs",
            "max_chars": 6000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(focus.is_error, Some(true), "meaning_focus should succeed");
    assert!(
        tool_text(&focus)?.contains("A: meaning_focus"),
        "expected meaning_focus answer line"
    );

    // 15) `evidence_fetch` should accept pointers without hashes.
    let evidence = call_tool(
        &service,
        "evidence_fetch",
        serde_json::json!({
            "items": [
                { "file": "src/main.rs", "start_line": 1, "end_line": 3, "source_hash": null }
            ],
            "max_chars": 4000,
            "max_lines": 20,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        evidence.is_error,
        Some(true),
        "evidence_fetch should succeed"
    );
    assert!(
        tool_text(&evidence)?.contains("R: src/main.rs:1"),
        "expected evidence file ref"
    );

    // 16) Cursor fail-closed: a cursor from one tool should not be accepted by another.
    let list_small = call_tool(
        &service,
        "ls",
        serde_json::json!({
            "limit": 1,
            "max_chars": 4000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(list_small.is_error, Some(true), "ls should succeed");
    let cursor = extract_cursor(tool_text(&list_small)?).context("expected ls to return cursor")?;
    let wrong_cursor = call_tool(
        &service,
        "cat",
        serde_json::json!({
            "cursor": cursor,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_error_code(&wrong_cursor, "invalid_cursor")?;

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
