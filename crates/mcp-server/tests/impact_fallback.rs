use anyhow::{Context, Result};
use context_code_chunker::{ChunkMetadata, ChunkType, CodeChunk};
use context_vector_store::ChunkCorpus;
use rmcp::{model::CallToolRequestParam, service::ServiceExt, transport::TokioChildProcess};
use serde_json::Value;
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
async fn impact_falls_back_to_text_matches_when_graph_has_no_edges() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("lib.rs"),
        "macro_rules! foo { () => {} }\nfn caller() { foo!(); }\n",
    )
    .context("write lib.rs")?;

    // Create a minimal corpus + index; no embedding model required for impact fallback.
    std::fs::create_dir_all(
        root.join(".context-finder")
            .join("indexes")
            .join("bge-small"),
    )
    .context("mkdir indexes")?;

    let mut corpus = ChunkCorpus::new();
    corpus.set_file_chunks(
        "src/lib.rs".to_string(),
        vec![
            CodeChunk::new(
                "src/lib.rs".to_string(),
                1,
                1,
                "macro_rules! foo { () => {} }\n".to_string(),
                ChunkMetadata::default()
                    .symbol_name("foo")
                    .chunk_type(ChunkType::Function),
            ),
            CodeChunk::new(
                "src/lib.rs".to_string(),
                2,
                2,
                "fn caller() { foo!(); }\n".to_string(),
                ChunkMetadata::default()
                    .symbol_name("caller")
                    .chunk_type(ChunkType::Function),
            ),
        ],
    );
    corpus
        .save(root.join(".context-finder").join("corpus.json"))
        .await
        .context("save corpus")?;

    std::fs::write(
        root.join(".context-finder")
            .join("indexes")
            .join("bge-small")
            .join("index.json"),
        r#"{"schema_version":3,"dimension":384,"next_id":2,"id_map":{"0":"src/lib.rs:1:1","1":"src/lib.rs:2:2"},"vectors":{}}"#,
    )
    .context("write index.json")?;

    let impact_args = serde_json::json!({
        "symbol": "foo",
        "path": root.to_string_lossy(),
        "depth": 2,
        "language": "rust",
        "auto_index": false,
    });
    let impact_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "impact".into(),
            arguments: impact_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling impact")??;

    assert_ne!(impact_result.is_error, Some(true), "impact returned error");
    let impact_text = impact_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("impact did not return text content")?;
    let impact_json: Value =
        serde_json::from_str(impact_text).context("impact output is not valid JSON")?;

    let total_usages = impact_json
        .get("total_usages")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    assert!(
        total_usages >= 1,
        "expected fallback usage, got: {impact_json}"
    );

    let direct = impact_json
        .get("direct")
        .and_then(Value::as_array)
        .context("direct missing")?;
    assert!(
        direct.iter().any(|v| {
            v.get("relationship")
                .and_then(Value::as_str)
                .is_some_and(|r| r == "TextMatch")
        }),
        "expected TextMatch usage, got: {direct:?}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn impact_returns_text_matches_when_symbol_is_missing_from_graph() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    // Create a minimal corpus + index; no embedding model required.
    std::fs::create_dir_all(
        root.join(".context-finder")
            .join("indexes")
            .join("bge-small"),
    )
    .context("mkdir indexes")?;

    let mut corpus = ChunkCorpus::new();
    corpus.set_file_chunks(
        "src/lib.rs".to_string(),
        vec![CodeChunk::new(
            "src/lib.rs".to_string(),
            1,
            3,
            "fn caller() {\n  let x = FOO_UNKNOWN + 1;\n}\n".to_string(),
            ChunkMetadata::default().symbol_name("caller"),
        )],
    );
    corpus
        .save(root.join(".context-finder").join("corpus.json"))
        .await
        .context("save corpus")?;

    std::fs::write(
        root.join(".context-finder")
            .join("indexes")
            .join("bge-small")
            .join("index.json"),
        r#"{"schema_version":3,"dimension":384,"next_id":1,"id_map":{"0":"src/lib.rs:1:3"},"vectors":{}}"#,
    )
    .context("write index.json")?;

    let impact_args = serde_json::json!({
        "symbol": "FOO_UNKNOWN",
        "path": root.to_string_lossy(),
        "depth": 2,
        "language": "rust",
        "auto_index": false,
    });
    let impact_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "impact".into(),
            arguments: impact_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling impact")??;

    assert_ne!(impact_result.is_error, Some(true), "impact returned error");
    let impact_text = impact_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("impact did not return text content")?;
    let impact_json: Value =
        serde_json::from_str(impact_text).context("impact output is not valid JSON")?;

    let direct = impact_json
        .get("direct")
        .and_then(Value::as_array)
        .context("direct missing")?;
    assert!(
        direct.iter().any(|v| {
            v.get("relationship")
                .and_then(Value::as_str)
                .is_some_and(|r| r == "TextMatch")
        }),
        "expected TextMatch usage, got: {direct:?}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
