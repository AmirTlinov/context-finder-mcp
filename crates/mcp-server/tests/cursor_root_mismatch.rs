use anyhow::{Context, Result};
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
async fn list_files_cursor_root_mismatch_includes_details() -> Result<()> {
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
        "list_files",
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
        "expected list_files on root1 to succeed"
    );
    assert!(
        list1.structured_content.is_none(),
        "list_files should not return structured_content"
    );
    let list1_text = list1
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("list_files(root1) missing text output")?;
    let cursor = list1_text
        .lines()
        .find_map(|line| line.strip_prefix("M: ").map(str::trim))
        .map(str::to_string)
        .context("list_files(root1) missing M: cursor (expected pagination)")?;

    let list2 = call_tool_allow_error(
        &service,
        "list_files",
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
        "expected list_files on root2 with root1 cursor to error"
    );

    assert!(
        list2.structured_content.is_none(),
        "list_files should not return structured_content on error"
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
