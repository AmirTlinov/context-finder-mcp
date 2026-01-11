use anyhow::{Context, Result};
use context_code_chunker::{ChunkMetadata, CodeChunk};
use context_vector_store::{context_dir_for_project_root, ChunkCorpus};
use rmcp::{model::CallToolRequestParam, service::ServiceExt, transport::TokioChildProcess};
use std::collections::HashSet;
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
async fn mcp_exposes_core_tools_and_map_has_no_side_effects() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tools = tokio::time::timeout(
        Duration::from_secs(10),
        service.list_tools(Default::default()),
    )
    .await
    .context("timeout listing tools")??;
    let tools_raw =
        serde_json::to_vec(&tools).context("serialize tools/list response for diagnostics")?;
    let tool_names: HashSet<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in [
        "capabilities",
        "help",
        "map",
        "repo_onboarding_pack",
        "read_pack",
        "file_slice",
        "list_files",
        "grep_context",
        "batch",
        "doctor",
        "search",
        "context",
        "context_pack",
        "text_search",
        "impact",
        "trace",
        "explain",
        "overview",
    ] {
        assert!(
            tool_names.contains(expected),
            "missing tool '{expected}' (available: {tool_names:?})"
        );
    }
    // Keep the tools/list payload reasonably sized so MCP clients don't choke on it.
    assert!(
        tools_raw.len() < 1_500_000,
        "tools/list payload is unexpectedly large ({} bytes)",
        tools_raw.len()
    );

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("main.rs"),
        "fn main() { println!(\"hi\"); }\n",
    )
    .context("write main.rs")?;

    let context_dir = context_dir_for_project_root(root);
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "temp project unexpectedly has a context dir before map"
    );

    let map_args = serde_json::json!({
        "path": root.to_string_lossy(),
        "depth": 2,
        "limit": 20,
        "response_mode": "facts",
    });
    let map_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "map".into(),
            arguments: map_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling map")??;

    assert_ne!(map_result.is_error, Some(true), "map returned error");
    let map_text = map_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("map missing text output")?;
    assert!(map_text.contains("map:"), "map output missing summary");
    assert!(
        map_text.contains("src"),
        "expected src directory to appear in map output"
    );

    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "map created project context side effects"
    );

    let doctor_args =
        serde_json::json!({ "path": root.to_string_lossy(), "response_mode": "full" });
    let doctor_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "doctor".into(),
            arguments: doctor_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling doctor")??;

    assert_ne!(doctor_result.is_error, Some(true), "doctor returned error");
    let doctor_text = doctor_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("doctor missing text output")?;
    assert!(
        doctor_text.contains("profile: quality"),
        "expected doctor output to mention profile"
    );
    assert!(
        doctor_text.contains("project_root:"),
        "expected doctor output to mention project_root"
    );
    assert!(
        doctor_text.contains(root.to_string_lossy().as_ref()),
        "expected doctor output to include the requested root"
    );

    // Create a minimal corpus + index to validate drift diagnostics without requiring embedding models.
    let context_dir = context_dir_for_project_root(root);
    std::fs::create_dir_all(context_dir.join("indexes").join("bge-small"))
        .context("mkdir indexes")?;

    let mut corpus = ChunkCorpus::new();
    corpus.set_file_chunks(
        "src/main.rs".to_string(),
        vec![CodeChunk::new(
            "src/main.rs".to_string(),
            1,
            1,
            "fn main() {}".to_string(),
            ChunkMetadata::default(),
        )],
    );
    corpus
        .save(context_dir.join("corpus.json"))
        .await
        .context("save corpus")?;

    std::fs::write(
        context_dir
            .join("indexes")
            .join("bge-small")
            .join("index.json"),
        r#"{"schema_version":3,"dimension":384,"next_id":1,"id_map":{"0":"src/other.rs:1:1"},"vectors":{}}"#,
    )
    .context("write index.json")?;

    let doctor_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "doctor".into(),
            arguments: doctor_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling doctor (with corpus/index)")??;

    assert_ne!(
        doctor_result.is_error,
        Some(true),
        "doctor returned error (with corpus/index)"
    );
    let doctor_text = doctor_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("doctor (with corpus/index) missing text output")?;
    assert!(
        doctor_text.contains("has_corpus=true"),
        "expected doctor to report has_corpus=true after corpus.json is present"
    );

    // Batch: one call → multiple tools, with a single bounded JSON output.
    let batch_args = serde_json::json!({
        "path": root.to_string_lossy(),
        "max_chars": 20000,
        "items": [
            { "id": "map", "tool": "map", "input": { "depth": 2, "limit": 20 } },
            { "id": "doctor", "tool": "doctor", "input": {} }
        ]
    });

    let batch_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "batch".into(),
            arguments: batch_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling batch")??;

    assert_ne!(batch_result.is_error, Some(true), "batch returned error");
    assert!(
        batch_result.structured_content.is_none(),
        "batch should not return structured_content"
    );
    let batch_text = batch_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("batch missing text output")?;
    assert!(
        batch_text.contains("batch: items=2 truncated=false"),
        "batch output missing summary"
    );
    assert!(
        batch_text.contains("item map: tool=map status=ok"),
        "batch output missing map item status"
    );
    assert!(
        batch_text.contains("item doctor: tool=doctor status=ok"),
        "batch output missing doctor item status"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn mcp_batch_truncates_when_budget_is_too_small() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(
        root.join("src").join("main.rs"),
        "fn main() { println!(\"hi\"); }\n",
    )
    .context("write main.rs")?;

    let batch_args = serde_json::json!({
        "path": root.to_string_lossy(),
        "max_chars": 200,
        "items": [
            { "id": "doctor", "tool": "doctor", "input": {} }
        ]
    });

    let batch_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "batch".into(),
            arguments: batch_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling batch (truncation)")??;

    assert_eq!(
        batch_result.is_error,
        Some(true),
        "batch should return error"
    );
    assert!(
        batch_result.structured_content.is_none(),
        "batch should not return structured_content on error"
    );
    let text = batch_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or_default();
    assert!(
        text.contains("error: invalid_request"),
        "expected invalid_request error, got: {text}"
    );
    assert!(
        text.contains("max_chars too small"),
        "expected max_chars guidance, got: {text}"
    );
    assert!(
        text.contains("next: batch"),
        "expected next action hint in error text, got: {text}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn mcp_file_slice_reads_bounded_lines_and_rejects_escape() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(root.join("src").join("main.rs"), "line-1\nline-2\nline-3\n")
        .context("write main.rs")?;

    let context_dir = context_dir_for_project_root(root);
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "temp project unexpectedly has a context dir before file_slice"
    );

    let slice_args = serde_json::json!({
        "path": root.to_string_lossy(),
        "file": "src/main.rs",
        "start_line": 2,
        "max_lines": 2,
        "max_chars": 2000,
    });
    let slice_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "file_slice".into(),
            arguments: slice_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling file_slice")??;

    assert_ne!(
        slice_result.is_error,
        Some(true),
        "file_slice returned error"
    );
    let slice_text = slice_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("file_slice missing text output")?;
    assert!(slice_text.contains("R: src/main.rs:2"));
    assert!(slice_text.contains("line-2"));
    assert!(slice_text.contains("line-3"));
    assert!(
        !slice_text.contains("line-1"),
        "did not expect line-1 when start_line=2"
    );

    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "file_slice created project context side effects"
    );

    let outside_parent = root.parent().context("temp root has no parent")?;
    let outside = tempfile::NamedTempFile::new_in(outside_parent).context("outside temp file")?;
    std::fs::write(outside.path(), "nope").context("write outside file")?;
    let outside_name = outside
        .path()
        .file_name()
        .context("outside temp file has no file name")?
        .to_string_lossy()
        .into_owned();

    let escape_args = serde_json::json!({
        "path": root.to_string_lossy(),
        "file": format!("../{outside_name}"),
        "start_line": 1,
        "max_lines": 10,
        "max_chars": 2000,
    });
    let escape_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "file_slice".into(),
            arguments: escape_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling file_slice (escape)")??;

    assert_eq!(
        escape_result.is_error,
        Some(true),
        "file_slice escape should error"
    );
    let escape_text = escape_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or_default();
    assert!(
        escape_text.contains("outside project root"),
        "unexpected escape error message: {escape_text}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn mcp_list_files_lists_paths_and_is_bounded() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")??;

    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").context("write main.rs")?;
    std::fs::create_dir_all(root.join("docs")).context("mkdir docs")?;
    std::fs::write(root.join("docs").join("README.md"), "# Hello\n").context("write docs")?;
    std::fs::write(root.join("README.md"), "Root\n").context("write root readme")?;

    let context_dir = context_dir_for_project_root(root);
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "temp project unexpectedly has a context dir before list_files"
    );

    let list_args = serde_json::json!({
        "path": root.to_string_lossy(),
        "file_pattern": "src/*",
        "limit": 50,
        "max_chars": 20_000,
    });
    let list_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "list_files".into(),
            arguments: list_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling list_files")??;

    assert_ne!(
        list_result.is_error,
        Some(true),
        "list_files returned error"
    );
    let list_text = list_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("list_files missing text output")?;
    assert!(list_text.contains("src/main.rs"));
    assert!(
        !list_text.contains("\nM: "),
        "did not expect truncation cursor for list_files"
    );

    let limited_args = serde_json::json!({
        "path": root.to_string_lossy(),
        "limit": 1,
        "max_chars": 20_000,
    });
    let limited_result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: "list_files".into(),
            arguments: limited_args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling list_files (limited)")??;
    assert_ne!(
        limited_result.is_error,
        Some(true),
        "list_files (limited) returned error"
    );
    let limited_text = limited_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("list_files (limited) missing text output")?;
    assert!(
        limited_text.contains("\nM: "),
        "expected truncation cursor (M:)"
    );

    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists(),
        "list_files created project context side effects"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
