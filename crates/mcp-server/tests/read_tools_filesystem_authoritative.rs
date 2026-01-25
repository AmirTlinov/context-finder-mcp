use anyhow::{Context, Result};
use context_code_chunker::{ChunkMetadata, CodeChunk};
use context_vector_store::{context_dir_for_project_root, ChunkCorpus};
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

#[tokio::test]
async fn read_tools_use_filesystem_even_when_corpus_is_partial() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::create_dir_all(root.join("ai")).context("mkdir ai")?;

    std::fs::write(
        root.join("src").join("main.rs"),
        "fn main() { /* needle */ }\n",
    )
    .context("write src/main.rs")?;
    std::fs::write(root.join("ai").join("protocol.yaml"), "name: demo\n")
        .context("write ai/protocol.yaml")?;

    // Create a partial corpus that only contains `ai/protocol.yaml`.
    // The read tools must still operate over the filesystem and must not silently ignore
    // `src/main.rs` just because it isn't present in the corpus.
    let mut corpus = ChunkCorpus::new();
    corpus.set_file_chunks(
        "ai/protocol.yaml".to_string(),
        vec![CodeChunk::new(
            "ai/protocol.yaml".to_string(),
            1,
            1,
            "name: demo".to_string(),
            ChunkMetadata::default(),
        )],
    );
    let context_dir = context_dir_for_project_root(root);
    std::fs::create_dir_all(&context_dir).context("mkdir project context dir")?;
    corpus
        .save(context_dir.join("corpus.json"))
        .await
        .context("save corpus")?;

    // find should include src/main.rs even though corpus does not.
    let list_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "find".into(),
            arguments: serde_json::json!({
                "path": root.to_string_lossy(),
                "limit": 50,
                "max_chars": 20_000,
                "response_mode": "minimal",
            })
            .as_object()
            .cloned(),
        }),
    )
    .await
    .context("timeout calling find")??;
    assert_ne!(list_result.is_error, Some(true), "find returned error");
    assert!(
        list_result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.contains("src/main.rs"))
            .unwrap_or(false),
        "expected src/main.rs in ls output"
    );

    // rg should find the needle in src/main.rs without requiring file_pattern hints.
    let grep_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "rg".into(),
            arguments: serde_json::json!({
                "path": root.to_string_lossy(),
                "pattern": "needle",
                "context": 1,
                "max_chars": 20_000,
                "response_mode": "minimal",
            })
            .as_object()
            .cloned(),
        }),
    )
    .await
    .context("timeout calling rg")??;
    assert_ne!(grep_result.is_error, Some(true), "rg returned error");
    assert!(
        grep_result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.contains("src/main.rs") && t.text.contains("needle"))
            .unwrap_or(false),
        "expected rg to return a match from src/main.rs"
    );

    // text_search should find the needle in src/main.rs even when a corpus exists.
    let search_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "text_search".into(),
            arguments: serde_json::json!({
                "path": root.to_string_lossy(),
                "pattern": "needle",
                "max_results": 10,
                "max_chars": 20_000,
                "response_mode": "minimal",
            })
            .as_object()
            .cloned(),
        }),
    )
    .await
    .context("timeout calling text_search")??;
    assert_ne!(
        search_result.is_error,
        Some(true),
        "text_search returned error"
    );
    assert!(
        search_result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.contains("src/main.rs") && t.text.contains("needle"))
            .unwrap_or(false),
        "expected text_search to include match from src/main.rs"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
