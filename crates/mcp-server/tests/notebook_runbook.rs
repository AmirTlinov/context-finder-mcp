use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RunningService, Service, ServiceExt},
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

async fn start_mcp_server(
) -> Result<RunningService<rmcp::RoleClient, impl Service<rmcp::RoleClient>>> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    cmd.env("RUST_LOG", "warn");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")
}

async fn call_tool_text(
    service: &RunningService<rmcp::RoleClient, impl Service<rmcp::RoleClient>>,
    name: &str,
    args: serde_json::Value,
) -> Result<String> {
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
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("tool did not return text content")?;
    Ok(text.to_string())
}

fn extract_cursor(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("M: ").map(|v| v.trim().to_string()))
        .filter(|v| !v.is_empty())
}

fn extract_note_value(text: &str, key: &str) -> Option<String> {
    let prefix = format!("N: {key}=");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix).map(|v| v.trim().to_string()))
        .filter(|v| !v.is_empty())
}

#[tokio::test]
async fn notebook_and_runbook_basic_flow() -> Result<()> {
    let service = start_mcp_server().await?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src/main.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    )
    .context("write main.rs")?;

    let edit = serde_json::json!({
        "version": 1,
        "path": root.to_string_lossy().to_string(),
        "ops": [
            {
                "op": "upsert_anchor",
                "anchor": {
                    "id": "a1",
                    "kind": "entrypoint",
                    "label": "Main entry",
                    "evidence": [
                        {"file": "src/main.rs", "start_line": 1, "end_line": 3}
                    ]
                }
            },
            {
                "op": "upsert_runbook",
                "runbook": {
                    "id": "rb1",
                    "title": "Core refresh",
                    "sections": [
                        {"id": "s1", "kind": "anchors", "title": "Hot spots", "anchor_ids": ["a1"], "include_evidence": true}
                    ]
                }
            }
        ]
    });
    let _ = call_tool_text(&service, "notebook_edit", edit).await?;

    let pack = call_tool_text(
        &service,
        "notebook_pack",
        serde_json::json!({
            "path": root.to_string_lossy().to_string(),
            "max_chars": 2000
        }),
    )
    .await?;
    assert!(
        pack.contains("a1"),
        "notebook_pack should mention anchor id"
    );
    assert!(
        pack.contains("rb1"),
        "notebook_pack should mention runbook id"
    );

    let toc = call_tool_text(
        &service,
        "runbook_pack",
        serde_json::json!({
            "path": root.to_string_lossy().to_string(),
            "runbook_id": "rb1",
            "mode": "summary",
            "max_chars": 2000
        }),
    )
    .await?;
    assert!(toc.contains("toc:"), "runbook_pack should include toc");
    assert!(
        toc.contains("s1"),
        "runbook_pack toc should include section id"
    );

    let expanded = call_tool_text(
        &service,
        "runbook_pack",
        serde_json::json!({
            "path": root.to_string_lossy().to_string(),
            "runbook_id": "rb1",
            "mode": "section",
            "section_id": "s1",
            "max_chars": 900
        }),
    )
    .await?;
    assert!(
        expanded.contains("ANCHOR"),
        "expanded section should include anchor content"
    );

    // If truncated, ensure continuation cursor works.
    if let Some(cursor) = extract_cursor(&expanded) {
        let cont = call_tool_text(
            &service,
            "runbook_pack",
            serde_json::json!({
                "path": root.to_string_lossy().to_string(),
                "runbook_id": "rb1",
                "cursor": cursor,
                "max_chars": 900
            }),
        )
        .await?;
        assert!(
            cont.contains("ANCHOR") || cont.contains("EV"),
            "continuation should return content"
        );
    }

    Ok(())
}

#[tokio::test]
async fn notebook_apply_suggest_preview_apply_rollback() -> Result<()> {
    let service = start_mcp_server().await?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src/main.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    )
    .context("write main.rs")?;

    let suggest = call_tool_text(
        &service,
        "notebook_suggest",
        serde_json::json!({
            "path": root.to_string_lossy().to_string(),
            "max_chars": 800,
            "response_mode": "minimal"
        }),
    )
    .await?;
    let repo_id = extract_note_value(&suggest, "repo_id").context("missing repo_id")?;

    let suggestion = serde_json::json!({
        "version": 1,
        "repo_id": repo_id,
        "query": "test",
        "anchors": [
            {
                "id": "a1",
                "kind": "entrypoint",
                "label": "Main entry",
                "evidence": [
                    {"file": "src/main.rs", "start_line": 1, "end_line": 3}
                ]
            }
        ],
        "runbooks": [
            {
                "id": "rb_test",
                "title": "Test runbook",
                "sections": [
                    {"id": "s1", "kind": "anchors", "title": "Hot spots", "anchor_ids": ["a1"], "include_evidence": true}
                ]
            }
        ],
        "budget": { "max_chars": 2000, "used_chars": 0, "truncated": false }
    });

    let preview = call_tool_text(
        &service,
        "notebook_apply_suggest",
        serde_json::json!({
            "version": 1,
            "mode": "preview",
            "path": root.to_string_lossy().to_string(),
            "scope": "project",
            "suggestion": suggestion.clone()
        }),
    )
    .await?;
    assert!(preview.contains("mode=Preview"), "expected preview mode");

    let apply = call_tool_text(
        &service,
        "notebook_apply_suggest",
        serde_json::json!({
            "version": 1,
            "mode": "apply",
            "path": root.to_string_lossy().to_string(),
            "scope": "project",
            "suggestion": suggestion
        }),
    )
    .await?;
    let backup_id = extract_note_value(&apply, "backup_id").context("missing backup_id")?;

    let pack = call_tool_text(
        &service,
        "notebook_pack",
        serde_json::json!({
            "path": root.to_string_lossy().to_string(),
            "max_chars": 2000
        }),
    )
    .await?;
    assert!(
        pack.contains("a1"),
        "notebook should contain applied anchor"
    );
    assert!(
        pack.contains("rb_test"),
        "notebook should contain applied runbook"
    );

    let _ = call_tool_text(
        &service,
        "notebook_apply_suggest",
        serde_json::json!({
            "version": 1,
            "mode": "rollback",
            "path": root.to_string_lossy().to_string(),
            "scope": "project",
            "backup_id": backup_id
        }),
    )
    .await?;

    let pack = call_tool_text(
        &service,
        "notebook_pack",
        serde_json::json!({
            "path": root.to_string_lossy().to_string(),
            "max_chars": 2000
        }),
    )
    .await?;
    assert!(
        !pack.contains("a1"),
        "notebook should not contain anchor after rollback"
    );
    assert!(
        !pack.contains("rb_test"),
        "notebook should not contain runbook after rollback"
    );

    Ok(())
}

#[tokio::test]
async fn notebook_suggest_and_noise_budget_are_low_noise_and_safe() -> Result<()> {
    let service = start_mcp_server().await?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src/main.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    )
    .context("write main.rs")?;
    std::fs::write(root.join("README.md"), "# Demo\n").context("write README.md")?;
    std::fs::create_dir_all(root.join(".github/workflows")).context("mkdir workflows")?;
    std::fs::write(
        root.join(".github/workflows/ci.yml"),
        "name: CI\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: echo ok\n",
    )
    .context("write ci.yml")?;

    // Autopilot suggestions should be bounded and always include a daily portal runbook.
    let suggested = call_tool_text(
        &service,
        "notebook_suggest",
        serde_json::json!({
            "path": root.to_string_lossy().to_string(),
            "max_chars": 1200
        }),
    )
    .await?;
    assert!(
        suggested.contains("notebook_suggest"),
        "tool output should include tool name"
    );
    assert!(
        suggested.contains("suggested_runbooks="),
        "tool output should include suggested runbooks count"
    );
    assert!(
        suggested.contains("Daily portal"),
        "tool output should include daily portal runbook"
    );

    // Enforce that runbook evidence does not overwhelm the output: when noise_budget is 0,
    // EV pointers are shown but snippet content is suppressed (fail-closed).
    let edit = serde_json::json!({
        "version": 1,
        "path": root.to_string_lossy().to_string(),
        "ops": [
            {
                "op": "upsert_anchor",
                "anchor": {
                    "id": "a1",
                    "kind": "entrypoint",
                    "label": "Main entry",
                    "evidence": [
                        {"file": "src/main.rs", "start_line": 1, "end_line": 3}
                    ]
                }
            },
            {
                "op": "upsert_runbook",
                "runbook": {
                    "id": "rb1",
                    "title": "Noise budget test",
                    "policy": {"noise_budget": 0.0},
                    "sections": [
                        {"id": "s1", "kind": "anchors", "title": "Hot spots", "anchor_ids": ["a1"], "include_evidence": true}
                    ]
                }
            }
        ]
    });
    let _ = call_tool_text(&service, "notebook_edit", edit).await?;

    let expanded = call_tool_text(
        &service,
        "runbook_pack",
        serde_json::json!({
            "path": root.to_string_lossy().to_string(),
            "runbook_id": "rb1",
            "mode": "section",
            "section_id": "s1",
            "max_chars": 900
        }),
    )
    .await?;
    assert!(
        expanded.contains("evidence content suppressed by runbook noise_budget"),
        "runbook_pack should suppress evidence snippets when noise_budget is zero"
    );

    Ok(())
}
