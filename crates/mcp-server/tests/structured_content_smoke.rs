use anyhow::{Context, Result};
use rmcp::{model::CallToolRequestParam, service::ServiceExt, transport::TokioChildProcess};
use std::time::Duration;
use tokio::process::Command;

mod support;

async fn call_tool(
    service: &rmcp::service::RunningService<
        rmcp::RoleClient,
        impl rmcp::service::Service<rmcp::RoleClient>,
    >,
    name: &str,
    args: serde_json::Value,
) -> Result<rmcp::model::CallToolResult> {
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
    Ok(result)
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
async fn core_tools_do_not_return_structured_content() -> Result<()> {
    let bin = support::locate_context_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");
    cmd.env("RUST_LOG", "warn");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"hi\"); }\n",
    )
    .context("write src/main.rs")?;
    std::fs::write(root.join("README.md"), "# Hello\n").context("write README.md")?;
    std::fs::create_dir_all(root.join("docs")).context("mkdir docs")?;
    std::fs::write(root.join("docs/README.md"), "# Docs\n").context("write docs/README.md")?;

    let tree = call_tool(
        &service,
        "tree",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "depth": 2,
            "limit": 20,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert!(
        tree.structured_content.is_none(),
        "tree should not return structured_content"
    );

    let list = call_tool(
        &service,
        "ls",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "limit": 50,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert!(
        list.structured_content.is_none(),
        "ls should not return structured_content"
    );

    let slice = call_tool(
        &service,
        "cat",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "file": "src/main.rs",
            "start_line": 1,
            "max_lines": 50,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert!(
        slice.structured_content.is_none(),
        "cat should not return structured_content"
    );

    let text_search = call_tool(
        &service,
        "text_search",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": "fn main",
            "max_results": 20,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert!(
        text_search.structured_content.is_none(),
        "text_search should not return structured_content"
    );

    let rg = call_tool(
        &service,
        "rg",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "pattern": r"fn\s+main",
            "context": 1,
            "max_hunks": 5,
            "max_chars": 5000,
            "format": "plain",
            "response_mode": "facts",
        }),
    )
    .await?;
    assert!(
        rg.structured_content.is_none(),
        "rg should not return structured_content"
    );

    let onboarding = call_tool(
        &service,
        "repo_onboarding_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "map_depth": 2,
            "map_limit": 10,
            "docs_limit": 5,
            "max_chars": 12000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert!(
        onboarding.structured_content.is_none(),
        "repo_onboarding_pack should not return structured_content"
    );

    let read_pack = call_tool(
        &service,
        "read_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "intent": "file",
            "file": "src/main.rs",
            "max_chars": 12000,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert!(
        read_pack.structured_content.is_none(),
        "read_pack should not return structured_content"
    );

    // Error responses should also include provenance in the text output so humans can eyeball
    // cross-project mixups even when the client UI hides structured content.
    let list_error = call_tool_allow_error(
        &service,
        "ls",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "cursor": "lol",
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_eq!(
        list_error.is_error,
        Some(true),
        "expected ls to return an error for invalid cursor"
    );
    let list_error_text = list_error
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or("");
    assert!(
        list_error_text.contains("root_fingerprint="),
        "expected error text to include root_fingerprint note"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
