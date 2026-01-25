use anyhow::{Context, Result};
use context_vector_store::context_dir_for_project_root;
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
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

async fn start_service() -> Result<RunningService<RoleClient, ()>> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("serve mcp server")
}

#[tokio::test]
async fn read_pack_query_includes_branchmind_external_memory_overlay() -> Result<()> {
    let service = start_service().await?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("main.rs"),
        "fn main() {\n  // placeholder\n}\n",
    )
    .context("write src/main.rs")?;

    let bm_dir = context_dir_for_project_root(root).join("branchmind");
    std::fs::create_dir_all(&bm_dir).context("mkdir branchmind dir")?;
    let bm_file = bm_dir.join("context_pack.json");

    let payload = serde_json::json!({
        "workspace": "ws1",
        "notes": { "entries": [
            { "seq": 1, "ts_ms": 1, "kind": "note", "title": "Decision: ports/adapters", "content": "Use ports/adapters for the integration." }
        ]},
        "trace": { "entries": [] },
        "signals": {
            "blockers": [],
            "evidence": [],
            "decisions": [
                { "id": "DEC-1", "type": "decision", "title": "Ports/adapters", "text": "Use ports/adapters for the integration.", "last_ts_ms": 2, "tags": [] }
            ]
        },
        "cards": [],
        "truncated": false
    });
    std::fs::write(
        &bm_file,
        serde_json::to_vec(&payload).context("serialize payload")?,
    )
    .context("write branchmind context_pack.json")?;

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "read_pack".into(),
            arguments: serde_json::json!({
                "path": root.to_string_lossy(),
                "intent": "query",
                "query": "ports adapters",
                "max_chars": 8000,
                "response_mode": "facts",
            })
            .as_object()
            .cloned(),
        }),
    )
    .await
    .context("timeout calling read_pack")??;

    assert_ne!(result.is_error, Some(true), "read_pack returned error");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("read_pack missing text output")?;

    assert!(
        text.contains("external_memory: source=branchmind"),
        "expected read_pack to include external_memory note"
    );
    assert!(
        text.contains("Ports/adapters"),
        "expected external memory overlay to include decision title"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
