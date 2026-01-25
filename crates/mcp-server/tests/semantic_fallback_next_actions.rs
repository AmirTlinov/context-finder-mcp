use anyhow::{Context, Result};
use context_code_chunker::{ChunkMetadata, ChunkType, CodeChunk};
use context_indexer::{compute_project_watermark, write_index_watermark};
use context_vector_store::{context_dir_for_project_root, StoredChunk};
use rmcp::{model::CallToolRequestParam, service::ServiceExt, transport::TokioChildProcess};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;

fn locate_context_finder_mcp_bin() -> Result<PathBuf> {
    if let Some(path) = option_env!("CARGO_BIN_EXE_context-finder-mcp") {
        return Ok(PathBuf::from(path));
    }

    // Cargo doesn't always expose CARGO_BIN_EXE_* at runtime. Derive it from the test exe path:
    // `.../target/{debug|release}/deps/<test>` → `.../target/{debug|release}/context-finder-mcp`
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
async fn search_full_mode_suggests_grep_context_when_semantic_disabled_and_no_hits() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    // Seed a minimal schema_version=1 semantic index for an unknown model id. The server can load
    // it, but embeddings will fail (unknown model) and must degrade to fuzzy-only search.
    let id = "src/main.rs:1:2";
    let chunk = CodeChunk::new(
        "src/main.rs".to_string(),
        1,
        2,
        "fn main() {}\n".to_string(),
        ChunkMetadata::default()
            .chunk_type(ChunkType::Function)
            .symbol_name("main"),
    );
    let stored = StoredChunk {
        chunk,
        vector: std::sync::Arc::new(vec![0.0, 0.0, 0.0]),
        id: id.to_string(),
        doc_hash: 0,
    };
    let mut chunks: HashMap<String, StoredChunk> = HashMap::new();
    chunks.insert(id.to_string(), stored);
    let mut id_map: HashMap<usize, String> = HashMap::new();
    id_map.insert(1, id.to_string());

    let index_dir = context_dir_for_project_root(root)
        .join("indexes")
        .join("unknown-model");
    std::fs::create_dir_all(&index_dir).context("mkdir index dir")?;
    std::fs::write(
        index_dir.join("index.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "dimension": 3,
            "chunks": chunks,
            "id_map": id_map
        }))?,
    )
    .context("write index.json")?;

    // Freshness-safe semantic tools require a watermark. Without it the index is treated as stale
    // and the server intentionally degrades to filesystem-only fallbacks.
    let watermark = compute_project_watermark(root)
        .await
        .context("compute project watermark")?;
    write_index_watermark(&index_dir.join("index.json"), watermark)
        .await
        .context("write watermark.json")?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_EMBEDDING_MODEL", "unknown-model");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")?;

    let args = serde_json::json!({
        "path": root.to_string_lossy(),
        "query": "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        "limit": 5,
        "response_mode": "full"
    });
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "search".into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling search")?
    .context("call search")?;

    assert_ne!(result.is_error, Some(true), "search returned error");

    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("search missing text output")?;
    assert!(
        text.contains("semantic: disabled"),
        "expected semantic disabled note in output: {text}"
    );

    assert!(
        text.contains("next: rg"),
        "expected rg hint to be printed in text output: {text}"
    );
    assert!(
        !text.contains("args="),
        "expected next_action hint to avoid embedding args in the text envelope: {text}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
