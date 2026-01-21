use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
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
    let err = focus_bad_format
        .expect_err("meaning_focus with invalid output_format should fail");
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
