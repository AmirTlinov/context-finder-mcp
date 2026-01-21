use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rmcp::{model::CallToolRequestParam, service::ServiceExt, transport::TokioChildProcess};
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;

fn extract_cursor_from_text(result: &rmcp::model::CallToolResult) -> Result<String> {
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("tool result missing text output")?;
    text.lines()
        .find_map(|line| line.strip_prefix("M: ").map(str::trim))
        .map(str::to_string)
        .context("tool result missing M: cursor")
}

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

async fn call_tool_allow_error(
    service: &rmcp::service::RunningService<
        rmcp::RoleClient,
        impl rmcp::service::Service<rmcp::RoleClient>,
    >,
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
async fn ls_cursor_root_mismatch_includes_details() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let root1 = tempfile::tempdir().context("tempdir root1")?;
    std::fs::create_dir_all(root1.path().join("src")).context("mkdir root1/src")?;
    for idx in 0..30 {
        std::fs::write(
            root1.path().join("src").join(format!("f{idx}.rs")),
            format!("pub fn f{idx}() -> usize {{ {idx} }}\n"),
        )
        .with_context(|| format!("write root1/src/f{idx}.rs"))?;
    }

    let root2 = tempfile::tempdir().context("tempdir root2")?;
    std::fs::create_dir_all(root2.path().join("src")).context("mkdir root2/src")?;
    std::fs::write(
        root2.path().join("src").join("other.rs"),
        "pub fn other() {}\n",
    )
    .context("write root2/src/other.rs")?;

    let list1 = call_tool_allow_error(
        &service,
        "ls",
        serde_json::json!({
            "path": root1.path().to_string_lossy(),
            "file_pattern": "src/*.rs",
            "limit": 5,
            "max_chars": 20_000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        list1.is_error,
        Some(true),
        "expected ls on root1 to succeed"
    );
    assert!(
        list1.structured_content.is_none(),
        "ls should not return structured_content"
    );
    let list1_text = list1
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("ls(root1) missing text output")?;
    let cursor = list1_text
        .lines()
        .find_map(|line| line.strip_prefix("M: ").map(str::trim))
        .map(str::to_string)
        .context("ls(root1) missing M: cursor (expected pagination)")?;

    let list2 = call_tool_allow_error(
        &service,
        "ls",
        serde_json::json!({
            "path": root2.path().to_string_lossy(),
            "cursor": cursor,
            "file_pattern": "src/*.rs",
            "limit": 5,
            "max_chars": 20_000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_eq!(
        list2.is_error,
        Some(true),
        "expected ls on root2 with root1 cursor to error"
    );

    assert!(
        list2.structured_content.is_none(),
        "ls should not return structured_content on error"
    );
    let list2_text = list2
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or_default();
    assert!(
        list2_text.contains("different root"),
        "expected root mismatch error, got: {list2_text}"
    );
    let expected_fp = list2_text
        .lines()
        .find_map(|line| line.strip_prefix("N: details.expected_root_fingerprint="))
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .context("details missing expected_root_fingerprint")?;
    let cursor_fp = list2_text
        .lines()
        .find_map(|line| line.strip_prefix("N: details.cursor_root_fingerprint="))
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .context("details missing cursor_root_fingerprint")?;
    assert_ne!(
        expected_fp, cursor_fp,
        "expected_root_fingerprint should differ from cursor_root_fingerprint"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn cat_cursor_only_does_not_switch_roots_when_session_root_is_set() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let root1 = tempfile::tempdir().context("tempdir root1")?;
    std::fs::write(root1.path().join("README.md"), "root1\none\ntwo\n")
        .context("write root1 README.md")?;

    let root2 = tempfile::tempdir().context("tempdir root2")?;
    std::fs::write(root2.path().join("README.md"), "root2\none\ntwo\nthree\n")
        .context("write root2 README.md")?;

    let res_root2 = call_tool_allow_error(
        &service,
        "cat",
        serde_json::json!({
            "path": root2.path().to_string_lossy(),
            "file": "README.md",
            "max_lines": 1,
            "max_chars": 2000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        res_root2.is_error,
        Some(true),
        "expected cat(root2) to succeed"
    );
    let cursor = extract_cursor_from_text(&res_root2)?;

    let res_root1 = call_tool_allow_error(
        &service,
        "cat",
        serde_json::json!({
            "path": root1.path().to_string_lossy(),
            "file": "README.md",
            "max_lines": 1,
            "max_chars": 2000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        res_root1.is_error,
        Some(true),
        "expected cat(root1) to succeed"
    );

    let res_foreign = call_tool_allow_error(
        &service,
        "cat",
        serde_json::json!({
            "cursor": cursor,
            "max_chars": 2000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_eq!(
        res_foreign.is_error,
        Some(true),
        "expected cat cursor-only root switch to error"
    );
    let res_text = res_foreign
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or_default();
    assert!(
        res_text.contains("different project root"),
        "expected cross-root cursor error, got: {res_text}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn rg_cursor_only_does_not_switch_roots_when_session_root_is_set() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let root1 = tempfile::tempdir().context("tempdir root1")?;
    std::fs::write(root1.path().join("README.md"), "root1\nneedle\n")
        .context("write root1 README.md")?;

    let root2 = tempfile::tempdir().context("tempdir root2")?;
    std::fs::create_dir_all(root2.path().join("src")).context("mkdir root2/src")?;
    std::fs::write(root2.path().join("src").join("a.txt"), "needle\n")
        .context("write root2/src/a.txt")?;
    std::fs::write(root2.path().join("src").join("b.txt"), "needle\n")
        .context("write root2/src/b.txt")?;

    let res_root2 = call_tool_allow_error(
        &service,
        "rg",
        serde_json::json!({
            "path": root2.path().to_string_lossy(),
            "pattern": "needle",
            "file_pattern": "src/*.txt",
            "max_hunks": 1,
            "max_chars": 20_000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        res_root2.is_error,
        Some(true),
        "expected rg(root2) to succeed"
    );
    let cursor = extract_cursor_from_text(&res_root2)?;

    let res_root1 = call_tool_allow_error(
        &service,
        "rg",
        serde_json::json!({
            "path": root1.path().to_string_lossy(),
            "pattern": "needle",
            "file_pattern": "README.md",
            "max_hunks": 1,
            "max_chars": 20_000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        res_root1.is_error,
        Some(true),
        "expected rg(root1) to succeed"
    );

    let res_foreign = call_tool_allow_error(
        &service,
        "rg",
        serde_json::json!({
            "cursor": cursor,
            "max_chars": 20_000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_eq!(
        res_foreign.is_error,
        Some(true),
        "expected rg cursor-only root switch to error"
    );
    let res_text = res_foreign
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or_default();
    assert!(
        res_text.contains("different project root"),
        "expected cross-root cursor error, got: {res_text}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn text_search_cursor_only_does_not_switch_roots_when_session_root_is_set() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let root1 = tempfile::tempdir().context("tempdir root1")?;
    std::fs::write(root1.path().join("README.md"), "root1\nneedle\n")
        .context("write root1 README.md")?;

    let root2 = tempfile::tempdir().context("tempdir root2")?;
    std::fs::create_dir_all(root2.path().join("src")).context("mkdir root2/src")?;
    for idx in 0..10 {
        std::fs::write(
            root2.path().join("src").join(format!("f{idx}.txt")),
            "needle\n",
        )
        .with_context(|| format!("write root2/src/f{idx}.txt"))?;
    }

    let res_root2 = call_tool_allow_error(
        &service,
        "text_search",
        serde_json::json!({
            "path": root2.path().to_string_lossy(),
            "pattern": "needle",
            "file_pattern": "src/*.txt",
            "max_results": 1,
            "max_chars": 20_000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        res_root2.is_error,
        Some(true),
        "expected text_search(root2) to succeed"
    );
    let cursor = extract_cursor_from_text(&res_root2)?;

    let res_root1 = call_tool_allow_error(
        &service,
        "text_search",
        serde_json::json!({
            "path": root1.path().to_string_lossy(),
            "pattern": "needle",
            "file_pattern": "README.md",
            "max_results": 1,
            "max_chars": 20_000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        res_root1.is_error,
        Some(true),
        "expected text_search(root1) to succeed"
    );

    let res_foreign = call_tool_allow_error(
        &service,
        "text_search",
        serde_json::json!({
            "cursor": cursor,
            "max_chars": 20_000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_eq!(
        res_foreign.is_error,
        Some(true),
        "expected text_search cursor-only root switch to error"
    );
    let res_text = res_foreign
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or_default();
    assert!(
        res_text.contains("different project root"),
        "expected cross-root cursor error, got: {res_text}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn map_cursor_only_does_not_switch_roots_when_session_root_is_set() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let root1 = tempfile::tempdir().context("tempdir root1")?;
    std::fs::create_dir_all(root1.path().join("src")).context("mkdir root1/src")?;
    std::fs::write(root1.path().join("src").join("a.rs"), "pub fn a() {}\n")
        .context("write root1/src/a.rs")?;

    let root2 = tempfile::tempdir().context("tempdir root2")?;
    std::fs::create_dir_all(root2.path().join("src")).context("mkdir root2/src")?;
    for idx in 0..20 {
        let dir = root2.path().join("src").join(format!("d{idx}"));
        std::fs::create_dir_all(&dir).with_context(|| format!("mkdir root2/src/d{idx}"))?;
        std::fs::write(dir.join("x.txt"), "x\n")
            .with_context(|| format!("write root2/src/d{idx}/x.txt"))?;
    }

    let res_root2 = call_tool_allow_error(
        &service,
        "tree",
        serde_json::json!({
            "path": root2.path().to_string_lossy(),
            "depth": 2,
            "limit": 1,
            "max_chars": 20_000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        res_root2.is_error,
        Some(true),
        "expected tree(root2) to succeed"
    );
    let cursor = extract_cursor_from_text(&res_root2)?;

    let res_root1 = call_tool_allow_error(
        &service,
        "tree",
        serde_json::json!({
            "path": root1.path().to_string_lossy(),
            "depth": 2,
            "limit": 1,
            "max_chars": 20_000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        res_root1.is_error,
        Some(true),
        "expected tree(root1) to succeed"
    );

    let res_foreign = call_tool_allow_error(
        &service,
        "tree",
        serde_json::json!({
            "cursor": cursor,
            "max_chars": 20_000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_eq!(
        res_foreign.is_error,
        Some(true),
        "expected tree cursor-only root switch to error"
    );
    let res_text = res_foreign
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or_default();
    assert!(
        res_text.contains("different project root"),
        "expected cross-root cursor error, got: {res_text}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn ls_cursor_only_does_not_switch_roots_when_session_root_is_set() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let root1 = tempfile::tempdir().context("tempdir root1")?;
    std::fs::create_dir_all(root1.path().join("src")).context("mkdir root1/src")?;
    std::fs::write(root1.path().join("src").join("a.rs"), "pub fn a() {}\n")
        .context("write root1/src/a.rs")?;
    std::fs::write(root1.path().join("src").join("b.rs"), "pub fn b() {}\n")
        .context("write root1/src/b.rs")?;

    let root2 = tempfile::tempdir().context("tempdir root2")?;
    std::fs::create_dir_all(root2.path().join("src")).context("mkdir root2/src")?;
    for idx in 0..5 {
        std::fs::write(
            root2.path().join("src").join(format!("f{idx}.rs")),
            format!("pub fn f{idx}() -> usize {{ {idx} }}\n"),
        )
        .with_context(|| format!("write root2/src/f{idx}.rs"))?;
    }

    let res_root2 = call_tool_allow_error(
        &service,
        "ls",
        serde_json::json!({
            "path": root2.path().to_string_lossy(),
            "file_pattern": "src/*.rs",
            "limit": 1,
            "max_chars": 20_000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        res_root2.is_error,
        Some(true),
        "expected ls(root2) to succeed"
    );
    let cursor = extract_cursor_from_text(&res_root2)?;

    let res_root1 = call_tool_allow_error(
        &service,
        "ls",
        serde_json::json!({
            "path": root1.path().to_string_lossy(),
            "file_pattern": "src/*.rs",
            "limit": 1,
            "max_chars": 20_000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        res_root1.is_error,
        Some(true),
        "expected ls(root1) to succeed"
    );

    let res_foreign = call_tool_allow_error(
        &service,
        "ls",
        serde_json::json!({
            "cursor": cursor,
            "max_chars": 20_000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_eq!(
        res_foreign.is_error,
        Some(true),
        "expected ls cursor-only root switch to error"
    );
    let res_text = res_foreign
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or_default();
    assert!(
        res_text.contains("different project root"),
        "expected cross-root cursor error, got: {res_text}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn read_pack_cursor_only_does_not_switch_roots_when_session_root_is_set() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let root1 = tempfile::tempdir().context("tempdir root1")?;
    std::fs::write(root1.path().join("README.md"), "root1\n").context("write root1 README.md")?;

    let root2 = tempfile::tempdir().context("tempdir root2")?;
    std::fs::write(root2.path().join("README.md"), "root2\n").context("write root2 README.md")?;

    // Establish a default root for the session.
    let first = call_tool_allow_error(
        &service,
        "read_pack",
        serde_json::json!({
            "path": root1.path().to_string_lossy(),
            "file": "README.md",
            "max_lines": 1,
            "max_chars": 2000,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_ne!(first.is_error, Some(true), "expected read_pack to succeed");

    // Simulate a cursor token from a different project root (e.g., pasted from another agent).
    let foreign_cursor_payload = serde_json::json!({
        "root": root2.path().to_string_lossy(),
    });
    let foreign_cursor_bytes =
        serde_json::to_vec(&foreign_cursor_payload).context("encode foreign cursor json")?;
    let foreign_cursor = URL_SAFE_NO_PAD.encode(foreign_cursor_bytes);

    let second = call_tool_allow_error(
        &service,
        "read_pack",
        serde_json::json!({
            "cursor": foreign_cursor,
            "max_chars": 2000,
            "response_mode": "minimal",
        }),
    )
    .await?;
    assert_eq!(
        second.is_error,
        Some(true),
        "expected read_pack cursor-only root switch to error"
    );
    let second_text = second
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or_default();
    assert!(
        second_text.contains("different project root"),
        "expected cross-root cursor error, got: {second_text}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
