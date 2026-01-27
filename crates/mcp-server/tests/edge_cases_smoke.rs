use anyhow::{Context, Result};
use context_vector_store::context_dir_for_project_root;
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::Duration;
use tokio::process::Command;

mod support;

async fn start_service(cwd: &Path) -> Result<RunningService<RoleClient, ()>> {
    let bin = support::locate_context_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.current_dir(cwd);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");

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

fn assert_not_internal_error(result: &rmcp::model::CallToolResult, tool: &str) -> Result<()> {
    if result.is_error == Some(true) {
        let text = tool_text(result)?;
        assert!(
            !text.contains("A: error: internal"),
            "{tool} returned internal error:\n{text}"
        );
    }
    Ok(())
}

fn run_git(root: &Path, args: &[&str]) -> Result<()> {
    let output = StdCommand::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "context-finder-tests")
        .env("GIT_AUTHOR_EMAIL", "tests@example.invalid")
        .env("GIT_COMMITTER_NAME", "context-finder-tests")
        .env("GIT_COMMITTER_EMAIL", "tests@example.invalid")
        .output()
        .with_context(|| format!("run git {args:?}"))?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git {args:?} failed (status={:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status.code()
        );
    }
    Ok(())
}

fn notebook_path_for_root(root: &Path) -> PathBuf {
    context_dir_for_project_root(root)
        .join("notebook")
        .join("notebook_v1.json")
}

fn load_notebook_json(root: &Path) -> Result<Value> {
    let path = notebook_path_for_root(root);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read notebook json {}", path.display()))?;
    serde_json::from_str(&raw).context("parse notebook json")
}

fn notebook_anchor_value<'a>(notebook: &'a Value, id: &str) -> Result<&'a Value> {
    notebook
        .get("anchors")
        .and_then(Value::as_array)
        .and_then(|anchors| {
            anchors
                .iter()
                .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
        })
        .with_context(|| format!("anchor not found in notebook: {id}"))
}

fn strip_suggest_fp_tag(anchor: &mut Value) {
    if let Some(tags) = anchor.get_mut("tags").and_then(Value::as_array_mut) {
        tags.retain(|t| {
            t.as_str()
                .map(|s| !s.starts_with("cf_suggest_fp="))
                .unwrap_or(true)
        });
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn compute_repo_id_fs(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    sha256_hex(canonical.to_string_lossy().as_bytes())
}

fn extract_backup_id(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("N: backup_id=")
            .map(str::trim)
            .map(str::to_string)
    })
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
    std::fs::write(
        root1_path.join("src").join("lib.rs"),
        "pub const LIB: &str = \"hi\";\n",
    )
    .context("write src/lib.rs")?;
    std::fs::write(root1_path.join("README.md"), "one\ntwo\nthree\n").context("write README.md")?;
    std::fs::write(
        root1_path.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .context("write Cargo.toml")?;
    std::fs::write(
        root1_path.join("AGENTS.md"),
        "AGENTS: test harness\n".repeat(400),
    )
    .context("write AGENTS.md")?;
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

    // 1) Default root inference: first call without `path` should work (ls is names-only).
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
    assert!(ls_text.contains("src"), "expected src directory in ls");
    assert!(
        ls_text.contains(".env"),
        "expected .env to appear in ls by default (names-only)"
    );
    assert!(
        !ls_text.contains("src/main.rs"),
        "did not expect recursive file paths in ls output"
    );

    // 1b) DX: once the session root is established, `ls` should treat a relative `path` as a
    // directory hint (common caller expectation) rather than switching the session root.
    std::fs::create_dir_all(root1_path.join("mcp_servers")).context("mkdir mcp_servers")?;
    std::fs::write(
        root1_path.join("mcp_servers").join("only_in_subdir.txt"),
        "hi\n",
    )
    .context("write mcp_servers/only_in_subdir.txt")?;
    let ls_dir_alias = call_tool(
        &service,
        "ls",
        serde_json::json!({
            "path": "mcp_servers",
            "limit": 50,
            "max_chars": 4000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(ls_dir_alias.is_error, Some(true), "ls should succeed");
    let ls_dir_text = tool_text(&ls_dir_alias)?;
    assert!(
        ls_dir_text.contains("only_in_subdir.txt"),
        "expected file in ls output for path-as-dir alias"
    );

    // Ensure the session root was not switched to the subdirectory.
    let ls_root_again = call_tool(
        &service,
        "ls",
        serde_json::json!({
            "limit": 50,
            "max_chars": 4000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(ls_root_again.is_error, Some(true), "ls should succeed");
    let ls_root_text = tool_text(&ls_root_again)?;
    assert!(
        ls_root_text.contains("mcp_servers"),
        "expected mcp_servers directory in root ls output"
    );
    assert!(
        !ls_root_text.contains("only_in_subdir.txt"),
        "did not expect subdir-only file in root ls output"
    );

    // 1c) UX: when `find` yields zero results, it should suggest directory tools (dirs vs files trap).
    std::fs::create_dir_all(root1_path.join("dir_only").join("nested"))
        .context("mkdir dir_only/nested")?;
    let find_dir_only = call_tool(
        &service,
        "find",
        serde_json::json!({
            "file_pattern": "dir_only",
            "limit": 50,
            "max_chars": 2000,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(
        find_dir_only.is_error,
        Some(true),
        "find dir_only should succeed"
    );
    let ls_dir_text = tool_text(&find_dir_only)?;
    assert!(
        ls_dir_text.contains("next: ls") || ls_dir_text.contains("next: ls or tree"),
        "expected find empty output to suggest ls/tree, got:\n{ls_dir_text}"
    );

    // 1b) Cursor rails: cursor-only continuation should work; mismatched params must fail-closed.
    let ls_src_page1 = call_tool(
        &service,
        "find",
        serde_json::json!({
            "file_pattern": "src",
            "limit": 1,
            "max_chars": 4000,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(
        ls_src_page1.is_error,
        Some(true),
        "find src page1 should succeed"
    );
    let ls_src_cursor = extract_cursor(tool_text(&ls_src_page1)?)
        .context("expected find cursor for src pagination")?;

    let ls_src_page2 = call_tool(
        &service,
        "find",
        serde_json::json!({ "cursor": ls_src_cursor.clone() }),
    )
    .await
    .context("find cursor-only continuation failed")?;
    assert_ne!(
        ls_src_page2.is_error,
        Some(true),
        "find src page2 should succeed"
    );
    assert!(
        tool_text(&ls_src_page2)?.contains("src/main.rs"),
        "expected src/main.rs on the second find page"
    );

    let ls_mismatch_file_pattern = call_tool(
        &service,
        "find",
        serde_json::json!({
            "cursor": ls_src_cursor.clone(),
            "file_pattern": "README",
        }),
    )
    .await?;
    assert_error_code(&ls_mismatch_file_pattern, "cursor_mismatch")?;

    let ls_mismatch_allow_secrets = call_tool(
        &service,
        "find",
        serde_json::json!({
            "cursor": ls_src_cursor,
            "allow_secrets": true,
        }),
    )
    .await?;
    assert_error_code(&ls_mismatch_allow_secrets, "cursor_mismatch")?;

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
        !cat_text.contains("\nR:"),
        "expected minimal cat output to suppress ref header noise"
    );
    assert!(
        cat_text.contains("fn main()"),
        "expected file content in cat"
    );
    assert!(
        !cat_text.contains("root_fingerprint="),
        "expected minimal cat output to suppress root_fingerprint noise"
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

    // 8) read_pack memory cursor should allow max_lines override (avoid strict cursor failures).
    let memory_page1 = call_tool(
        &service,
        "read_pack",
        serde_json::json!({
            "intent": "memory",
            "max_chars": 800,
            "response_mode": "facts"
        }),
    )
    .await?;
    assert_ne!(
        memory_page1.is_error,
        Some(true),
        "read_pack memory page1 should succeed"
    );
    let memory_cursor =
        extract_cursor(tool_text(&memory_page1)?).context("expected memory cursor in read_pack")?;

    let memory_page2 = call_tool(
        &service,
        "read_pack",
        serde_json::json!({
            "cursor": memory_cursor,
            "max_lines": 200
        }),
    )
    .await?;
    assert_ne!(
        memory_page2.is_error,
        Some(true),
        "read_pack memory cursor should accept max_lines override"
    );

    // 8) `tree` should also hide secrets.
    let tree = call_tool(
        &service,
        "tree",
        serde_json::json!({
            "depth": 3,
            "limit": 1,
            "max_chars": 4000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(tree.is_error, Some(true), "tree should succeed");
    let tree_text = tool_text(&tree)?;
    assert!(
        tree_text.contains("tree:"),
        "expected tree output header, got:\n{tree_text}"
    );
    assert!(
        tree_text.contains("src") || tree_text.contains("\n."),
        "expected src or root summary in tree, got:\n{tree_text}"
    );
    assert!(
        !tree_text.contains(".env"),
        "did not expect .env in tree, got:\n{tree_text}"
    );

    let tree_cursor = extract_cursor(tree_text).context("expected tree cursor")?;
    let tree_mismatch_depth = call_tool(
        &service,
        "tree",
        serde_json::json!({
            "cursor": tree_cursor,
            "depth": 4,
        }),
    )
    .await?;
    assert_error_code(&tree_mismatch_depth, "cursor_mismatch")?;

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

    // 13b) UX: missing required fields should include a hint with an example.
    let meaning_missing_query = call_tool(&service, "meaning_pack", serde_json::json!({})).await;
    let err = meaning_missing_query.expect_err("meaning_pack missing query should fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("missing field `query`"),
        "expected missing query error, got:\n{msg}"
    );
    assert!(
        msg.contains("Example:") && msg.contains("\"query\""),
        "expected example hint, got:\n{msg}"
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

    // 14a) UX: missing required focus should include a hint (and reuse common `path` mistakes).
    let focus_missing = call_tool(
        &service,
        "meaning_focus",
        serde_json::json!({
            "path": "src/agent",
        }),
    )
    .await;
    let err = focus_missing.expect_err("meaning_focus missing focus should fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("missing field `focus`"),
        "expected missing focus error, got:\n{msg}"
    );
    assert!(
        msg.contains("Example:") && msg.contains("\"focus\"") && msg.contains("src/agent"),
        "expected example hint for focus, got:\n{msg}"
    );

    // 14b) UX: invalid output_format should explain allowed values.
    let focus_bad_format = call_tool(
        &service,
        "meaning_focus",
        serde_json::json!({
            "focus": "src",
            "output_format": "md",
            "max_chars": 2000,
            "response_mode": "facts",
        }),
    )
    .await;
    let err = focus_bad_format.expect_err("meaning_focus with invalid output_format should fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("Invalid output_format 'md'"),
        "expected invalid output_format error, got:\n{msg}"
    );
    assert!(
        msg.contains("Allowed: context|markdown|context_and_diagram|diagram"),
        "expected allowed output_format values in error, got:\n{msg}"
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

#[tokio::test]
async fn edge_cases_smoke_pack_covers_all_tools_minimally() -> Result<()> {
    let root = tempfile::tempdir().context("tempdir root")?;
    let root_path = root.path();

    // Tiny, deterministic repo content to give every tool something to work with.
    std::fs::create_dir_all(root_path.join("src")).context("mkdir src")?;
    std::fs::write(
        root_path.join("src").join("lib.rs"),
        "pub fn beta() -> i32 { 2 }\n\npub fn alpha() -> i32 { beta() }\n",
    )
    .context("write src/lib.rs")?;
    std::fs::write(
        root_path.join("src").join("main.rs"),
        "fn main() {\n  println!(\"hi\");\n}\n",
    )
    .context("write src/main.rs")?;
    // Make README large enough to force pagination/cursor.
    let mut readme = String::new();
    for i in 0..300 {
        readme.push_str(&format!("line {i}\n"));
    }
    std::fs::write(root_path.join("README.md"), readme).context("write README.md")?;
    std::fs::create_dir_all(root_path.join(".github").join("workflows"))
        .context("mkdir .github/workflows")?;
    std::fs::write(
        root_path.join(".github").join("workflows").join("ci.yml"),
        "name: ci\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ok\n",
    )
    .context("write ci.yml")?;

    // Git + a worktree (exercise worktree_pack + .worktrees UX).
    std::fs::create_dir_all(root_path.join(".worktrees")).context("mkdir .worktrees")?;
    run_git(root_path, &["init", "-q"])?;
    run_git(root_path, &["add", "."])?;
    run_git(root_path, &["commit", "-m", "init", "-q"])?;
    run_git(
        root_path,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            ".worktrees/feature",
            "-q",
        ],
    )?;

    let service = start_service(root_path).await?;

    // Core discovery.
    let caps = call_tool(&service, "capabilities", serde_json::json!({})).await?;
    assert_ne!(caps.is_error, Some(true), "capabilities should succeed");

    let help = call_tool(&service, "help", serde_json::json!({})).await?;
    assert_ne!(help.is_error, Some(true), "help should succeed");
    assert!(
        tool_text(&help)?.contains("[LEGEND]"),
        "help should include [LEGEND]"
    );

    let tree = call_tool(
        &service,
        "tree",
        serde_json::json!({ "depth": 3, "limit": 50, "max_chars": 4000, "response_mode": "facts" }),
    )
    .await?;
    assert_ne!(tree.is_error, Some(true), "tree should succeed");

    let ls = call_tool(
        &service,
        "ls",
        serde_json::json!({ "limit": 100, "max_chars": 4000, "response_mode": "facts" }),
    )
    .await?;
    assert_ne!(ls.is_error, Some(true), "ls should succeed");

    let find = call_tool(
        &service,
        "find",
        serde_json::json!({ "limit": 20, "max_chars": 2000, "response_mode": "minimal" }),
    )
    .await?;
    assert_ne!(find.is_error, Some(true), "find should succeed");

    // Cursor ergonomics: continuing with a cursor must not depend on preserving max_lines.
    let cat_page1 = call_tool(
        &service,
        "cat",
        serde_json::json!({
            "file": "README.md",
            "start_line": 1,
            "max_lines": 1,
            "max_chars": 2000,
            "response_mode": "compact",
        }),
    )
    .await?;
    assert_ne!(cat_page1.is_error, Some(true), "cat should succeed");
    let cat_cursor = extract_cursor(tool_text(&cat_page1)?).context("expected cat cursor")?;
    let cat_page2 = call_tool(
        &service,
        "cat",
        serde_json::json!({
            "cursor": cat_cursor,
            // Intentional mismatch vs page1: max_lines changes.
            "max_lines": 2,
            "max_chars": 2000,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(
        cat_page2.is_error,
        Some(true),
        "cat cursor continuation should succeed"
    );

    // Text + regex search.
    let text_search = call_tool(
        &service,
        "text_search",
        serde_json::json!({
            "pattern": "fn main",
            "max_results": 10,
            "max_chars": 4000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        text_search.is_error,
        Some(true),
        "text_search should succeed"
    );

    let rg = call_tool(
        &service,
        "rg",
        serde_json::json!({
            "pattern": "pub fn",
            "literal": true,
            "max_hunks": 5,
            "max_chars": 4000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(rg.is_error, Some(true), "rg should succeed");

    let grep = call_tool(
        &service,
        "grep",
        serde_json::json!({
            "pattern": "pub fn",
            "literal": true,
            "max_hunks": 1,
            "max_chars": 2000,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(grep.is_error, Some(true), "grep alias should succeed");

    // Packs: onboarding + meaning.
    let onboarding = call_tool(
        &service,
        "repo_onboarding_pack",
        serde_json::json!({ "max_chars": 2000, "response_mode": "facts", "auto_index": false }),
    )
    .await?;
    assert_ne!(
        onboarding.is_error,
        Some(true),
        "repo_onboarding_pack should succeed"
    );

    let read_pack = call_tool(
        &service,
        "read_pack",
        serde_json::json!({
            "intent": "onboarding",
            "max_lines": 80,
            "max_chars": 3000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(read_pack.is_error, Some(true), "read_pack should succeed");

    // read_pack text UX: full mode should surface context_pack/onboarding summaries directly
    // (no references to structured_content, which isn't returned to clients).
    let read_pack_full = call_tool(
        &service,
        "read_pack",
        serde_json::json!({
            "intent": "query",
            "query": "alpha",
            "max_chars": 6000,
            "response_mode": "full",
        }),
    )
    .await?;
    assert_ne!(
        read_pack_full.is_error,
        Some(true),
        "read_pack full should succeed"
    );
    let read_pack_full_text = tool_text(&read_pack_full)?;
    assert!(
        read_pack_full_text.contains("context_pack:"),
        "read_pack full should include context_pack summary"
    );
    assert!(
        !read_pack_full_text.contains("structured_content"),
        "read_pack output must not mention structured_content"
    );

    let meaning_pack = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "query": "canon loop",
            "max_chars": 4000,
            "output_format": "markdown",
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        meaning_pack.is_error,
        Some(true),
        "meaning_pack should succeed"
    );

    let meaning_focus = call_tool(
        &service,
        "meaning_focus",
        serde_json::json!({
            "focus": "src",
            "max_chars": 4000,
            "output_format": "markdown",
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        meaning_focus.is_error,
        Some(true),
        "meaning_focus should succeed"
    );
    assert!(
        tool_text(&meaning_focus)?.contains("NBA evidence_fetch"),
        "meaning_focus should provide a copy/paste runnable evidence_fetch payload"
    );

    // Evidence fetch should work for a simple pointer.
    let evidence = call_tool(
        &service,
        "evidence_fetch",
        serde_json::json!({
            "items": [{ "file": "src/lib.rs", "start_line": 1, "end_line": 2, "source_hash": null }],
            "max_lines": 10,
            "max_chars": 2000,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(
        evidence.is_error,
        Some(true),
        "evidence_fetch should succeed"
    );

    // Diagnostics / semantics (must be usable without long delays).
    let doctor = call_tool(
        &service,
        "doctor",
        serde_json::json!({ "response_mode": "compact", "max_chars": 4000 }),
    )
    .await?;
    assert_ne!(doctor.is_error, Some(true), "doctor should succeed");

    for (tool, args) in [
        (
            "search",
            serde_json::json!({
                "query": "alpha",
                "limit": 5,
                "response_mode": "facts",
                "auto_index": false,
            }),
        ),
        (
            "context",
            serde_json::json!({
                "query": "alpha",
                "limit": 3,
                "response_mode": "facts",
                "auto_index": false,
            }),
        ),
        (
            "context_pack",
            serde_json::json!({
                "query": "alpha",
                "max_chars": 4000,
                "response_mode": "facts",
                "auto_index": false,
                "include_paths": ["src"],
            }),
        ),
        (
            "impact",
            serde_json::json!({
                "symbol": "alpha",
                "depth": 1,
                "response_mode": "facts",
                "auto_index": false,
            }),
        ),
        (
            "trace",
            serde_json::json!({
                "from": "alpha",
                "to": "beta",
                "response_mode": "facts",
                "auto_index": false,
            }),
        ),
        (
            "explain",
            serde_json::json!({
                "symbol": "alpha",
                "response_mode": "facts",
                "auto_index": false,
            }),
        ),
        (
            "overview",
            serde_json::json!({
                "response_mode": "facts",
                "auto_index": false,
            }),
        ),
    ] {
        let out = call_tool(&service, tool, args).await?;
        // We allow per-tool graceful degradation (invalid_request), but never internal errors.
        if out.is_error == Some(true) {
            let text = tool_text(&out)?;
            assert!(
                !text.contains("A: error: internal"),
                "{tool} returned internal error:\n{text}"
            );
        }
    }

    // Worktrees.
    let worktrees = call_tool(
        &service,
        "worktree_pack",
        serde_json::json!({ "limit": 20, "max_chars": 4000, "response_mode": "facts" }),
    )
    .await?;
    assert_ne!(
        worktrees.is_error,
        Some(true),
        "worktree_pack should succeed"
    );

    let atlas = call_tool(
        &service,
        "atlas_pack",
        serde_json::json!({ "max_chars": 6000, "response_mode": "facts" }),
    )
    .await?;
    assert_ne!(atlas.is_error, Some(true), "atlas_pack should succeed");

    // Notebook/runbook: one-click apply via batch v2 `$ref`, then load the portal.
    let apply_batch = call_tool(
        &service,
        "batch",
        serde_json::json!({
            "version": 2,
            "stop_on_error": true,
            "max_chars": 20000,
            "response_mode": "facts",
            "items": [
                {
                    "id": "suggest",
                    "tool": "notebook_suggest",
                    "input": { "query": "entrypoints ci", "max_chars": 800, "response_mode": "minimal" }
                },
                {
                    "id": "apply",
                    "tool": "notebook_apply_suggest",
                    "input": {
                        "version": 1,
                        "mode": "apply",
                        "scope": "project",
                        "allow_truncated": false,
                        "suggestion": { "$ref": "#/items/suggest/data" }
                    }
                }
            ]
        }),
    )
    .await?;
    assert_ne!(
        apply_batch.is_error,
        Some(true),
        "batch apply should succeed"
    );

    let notebook = call_tool(
        &service,
        "notebook_pack",
        serde_json::json!({ "max_chars": 4000, "response_mode": "facts" }),
    )
    .await?;
    assert_ne!(
        notebook.is_error,
        Some(true),
        "notebook_pack should succeed"
    );
    let notebook_text = tool_text(&notebook)?;
    let portal_id = notebook_text
        .lines()
        .find(|line| line.contains("Daily portal") && line.contains("id="))
        .and_then(|line| line.split("id=").nth(1))
        .and_then(|tail| tail.split(',').next())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .context("failed to extract Daily portal runbook id from notebook_pack")?;

    let runbook = call_tool(
        &service,
        "runbook_pack",
        serde_json::json!({
            "runbook_id": portal_id,
            "mode": "summary",
            "max_chars": 4000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(runbook.is_error, Some(true), "runbook_pack should succeed");

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn notebook_smoke_pack_is_gentle_and_rollbackable() -> Result<()> {
    let root = tempfile::tempdir().context("tempdir root")?;
    let root_path = root.path();

    // Minimal repo content so notebook_suggest has stable evidence targets.
    std::fs::create_dir_all(root_path.join("src")).context("mkdir src")?;
    std::fs::write(
        root_path.join("src").join("lib.rs"),
        "pub fn alpha() -> i32 { 1 }\n",
    )
    .context("write src/lib.rs")?;
    std::fs::write(
        root_path.join("src").join("main.rs"),
        "fn main() {\n  println!(\"hi\");\n}\n",
    )
    .context("write src/main.rs")?;
    std::fs::create_dir_all(root_path.join(".github").join("workflows"))
        .context("mkdir .github/workflows")?;
    std::fs::write(
        root_path.join(".github").join("workflows").join("ci.yml"),
        "name: ci\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ok\n",
    )
    .context("write ci.yml")?;

    let service = start_service(root_path).await?;

    // 1) Use a handcrafted suggestion payload (deterministic + no dependency on structured_content).
    let anchor_id = "entrypoint_main".to_string();
    let repo_id = compute_repo_id_fs(root_path);
    let suggestion = serde_json::json!({
        "version": 1,
        "repo_id": repo_id,
        "query": "smoke",
        "anchors": [
            {
                "id": anchor_id,
                "kind": "entrypoint",
                "label": "Entrypoint: main",
                "evidence": [
                    { "file": "src/main.rs", "start_line": 1, "end_line": 3, "source_hash": null }
                ],
                "locator": null,
                "tags": []
            }
        ],
        "runbooks": [
            {
                "id": "daily_portal",
                "title": "Daily portal",
                "sections": [
                    { "kind": "anchors", "id": "entry", "title": "Entrypoints", "anchor_ids": [anchor_id], "include_evidence": true }
                ],
                "tags": []
            }
        ],
        "budget": { "max_chars": 2000, "used_chars": 0, "truncated": false }
    });

    // 2) Create a manual (not managed) anchor with the same id.
    let mut manual_anchor = suggestion["anchors"][0].clone();
    strip_suggest_fp_tag(&mut manual_anchor);
    if let Some(obj) = manual_anchor.as_object_mut() {
        obj.insert(
            "label".to_string(),
            Value::String("manual anchor (do not overwrite)".to_string()),
        );
    }
    let edit_manual = call_tool(
        &service,
        "notebook_edit",
        serde_json::json!({
            "version": 1,
            "scope": "project",
            "ops": [
                { "op": "upsert_anchor", "anchor": manual_anchor }
            ]
        }),
    )
    .await?;
    assert_ne!(
        edit_manual.is_error,
        Some(true),
        "notebook_edit should succeed"
    );

    // 3) Safe preview should SKIP (not_managed).
    let preview_not_managed = call_tool(
        &service,
        "notebook_apply_suggest",
        serde_json::json!({
            "version": 1,
            "mode": "preview",
            "scope": "project",
            "overwrite_policy": "safe",
            "suggestion": suggestion,
        }),
    )
    .await?;
    assert_ne!(
        preview_not_managed.is_error,
        Some(true),
        "notebook_apply_suggest preview should succeed"
    );
    let preview_text = tool_text(&preview_not_managed)?;
    assert!(
        preview_text.contains("changes (preview):"),
        "expected preview change list, got:\n{preview_text}"
    );
    assert!(
        preview_text.contains(&format!("anchor {anchor_id}: skip (not_managed)")),
        "expected not_managed skip for anchor {anchor_id}, got:\n{preview_text}"
    );

    // 4) Force apply should overwrite and return a backup id.
    let apply_force = call_tool(
        &service,
        "notebook_apply_suggest",
        serde_json::json!({
            "version": 1,
            "mode": "apply",
            "scope": "project",
            "overwrite_policy": "force",
            "allow_truncated": false,
            "suggestion": suggestion.clone(),
        }),
    )
    .await?;
    assert_ne!(
        apply_force.is_error,
        Some(true),
        "notebook_apply_suggest apply(force) should succeed"
    );
    let backup_id =
        extract_backup_id(tool_text(&apply_force)?).context("expected backup_id on apply")?;
    assert!(!backup_id.trim().is_empty(), "backup_id must not be empty");

    // 5) Manually edit the now-managed anchor label (simulate human edits).
    let notebook_after_apply = load_notebook_json(root_path)?;
    let mut edited_anchor = notebook_anchor_value(&notebook_after_apply, &anchor_id)?.clone();
    if let Some(obj) = edited_anchor.as_object_mut() {
        obj.insert(
            "label".to_string(),
            Value::String("MANUAL EDIT (should cause skip)".to_string()),
        );
    }
    let edit_modified = call_tool(
        &service,
        "notebook_edit",
        serde_json::json!({
            "version": 1,
            "scope": "project",
            "ops": [
                { "op": "upsert_anchor", "anchor": edited_anchor }
            ]
        }),
    )
    .await?;
    assert_ne!(
        edit_modified.is_error,
        Some(true),
        "notebook_edit (manual edit) should succeed"
    );

    // 6) Safe preview should SKIP (manual_modified).
    let preview_manual_modified = call_tool(
        &service,
        "notebook_apply_suggest",
        serde_json::json!({
            "version": 1,
            "mode": "preview",
            "scope": "project",
            "overwrite_policy": "safe",
            "suggestion": suggestion.clone(),
        }),
    )
    .await?;
    assert_ne!(
        preview_manual_modified.is_error,
        Some(true),
        "notebook_apply_suggest preview should succeed"
    );
    let preview2_text = tool_text(&preview_manual_modified)?;
    assert!(
        preview2_text.contains(&format!("anchor {anchor_id}: skip (manual_modified)")),
        "expected manual_modified skip for anchor {anchor_id}, got:\n{preview2_text}"
    );

    // 7) Rollback should restore the pre-force snapshot (manual anchor, no suggest fp tag).
    let rollback = call_tool(
        &service,
        "notebook_apply_suggest",
        serde_json::json!({
            "version": 1,
            "mode": "rollback",
            "scope": "project",
            "backup_id": backup_id,
        }),
    )
    .await?;
    assert_ne!(
        rollback.is_error,
        Some(true),
        "notebook_apply_suggest rollback should succeed"
    );

    let notebook_after_rollback = load_notebook_json(root_path)?;
    let restored = notebook_anchor_value(&notebook_after_rollback, &anchor_id)?;
    let label = restored
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        label.contains("manual anchor"),
        "expected manual anchor label after rollback, got: {label}"
    );
    let tags: Vec<String> = restored
        .get("tags")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    assert!(
        tags.iter().all(|t| !t.starts_with("cf_suggest_fp=")),
        "expected no suggest fp tags after rollback, got: {tags:?}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn weird_repos_smoke_pack_is_stable_and_bounded() -> Result<()> {
    let base = tempfile::tempdir().context("tempdir base")?;
    let base_path = base.path();

    let dataset_repo = base_path.join("repo_dataset_heavy");
    let polyglot_repo = base_path.join("repo_polyglot");
    let nodocs_repo = base_path.join("repo_no_docs");

    // dataset-heavy: lots of noise + one small code entrypoint.
    std::fs::create_dir_all(dataset_repo.join("data")).context("mkdir dataset data")?;
    let mut csv = String::new();
    for i in 0..2000 {
        csv.push_str(&format!("{i},value_{i}\n"));
    }
    std::fs::write(dataset_repo.join("data").join("train.csv"), csv).context("write train.csv")?;
    std::fs::write(dataset_repo.join("data").join("blob.bin"), vec![0_u8; 8192])
        .context("write blob.bin")?;
    std::fs::create_dir_all(dataset_repo.join("src")).context("mkdir dataset src")?;
    std::fs::write(
        dataset_repo.join("src").join("main.py"),
        "def main():\n  print('hi')\n\nif __name__ == '__main__':\n  main()\n",
    )
    .context("write dataset src/main.py")?;

    // polyglot: multiple languages, minimal build markers.
    std::fs::create_dir_all(polyglot_repo.join("src")).context("mkdir polyglot src")?;
    std::fs::write(
        polyglot_repo.join("src").join("main.rs"),
        "fn main() { println!(\"hi\"); }\n",
    )
    .context("write polyglot main.rs")?;
    std::fs::write(
        polyglot_repo.join("src").join("index.ts"),
        "export const answer: number = 42;\n",
    )
    .context("write polyglot index.ts")?;
    std::fs::write(
        polyglot_repo.join("src").join("app.py"),
        "def run():\n  return 42\n",
    )
    .context("write polyglot app.py")?;
    std::fs::write(
        polyglot_repo.join("package.json"),
        "{ \"name\": \"poly\" }\n",
    )
    .context("write package.json")?;

    // no-docs: no README/CI hints; should still degrade gracefully.
    std::fs::create_dir_all(nodocs_repo.join("core")).context("mkdir nodocs core")?;
    std::fs::write(
        nodocs_repo.join("core").join("logic.rs"),
        "pub fn f() -> i32 { 1 }\n",
    )
    .context("write nodocs logic.rs")?;

    let service = start_service(base_path).await?;

    for (name, repo) in [
        ("dataset-heavy", &dataset_repo),
        ("polyglot", &polyglot_repo),
        ("no-docs", &nodocs_repo),
    ] {
        let repo_str = repo.to_string_lossy().to_string();

        let ls = call_tool(
            &service,
            "ls",
            serde_json::json!({ "path": repo_str, "limit": 50, "max_chars": 3000, "response_mode": "facts" }),
        )
        .await?;
        assert_not_internal_error(&ls, &format!("{name}: ls"))?;

        let onboarding = call_tool(
            &service,
            "repo_onboarding_pack",
            serde_json::json!({
                "path": repo.to_string_lossy(),
                "max_chars": 3000,
                "response_mode": "facts",
                "auto_index": false
            }),
        )
        .await?;
        assert_not_internal_error(&onboarding, &format!("{name}: repo_onboarding_pack"))?;

        let read_pack = call_tool(
            &service,
            "read_pack",
            serde_json::json!({
                "path": repo.to_string_lossy(),
                "intent": "onboarding",
                "max_lines": 120,
                "max_chars": 4000,
                "response_mode": "facts"
            }),
        )
        .await?;
        assert_not_internal_error(&read_pack, &format!("{name}: read_pack"))?;

        let meaning = call_tool(
            &service,
            "meaning_pack",
            serde_json::json!({
                "path": repo.to_string_lossy(),
                "query": "how to run tests",
                "max_chars": 4000,
                "response_mode": "facts"
            }),
        )
        .await?;
        assert_not_internal_error(&meaning, &format!("{name}: meaning_pack"))?;
    }

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
