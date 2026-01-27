use anyhow::{Context, Result};
use context_vector_store::context_dir_for_project_root;
use rmcp::{model::CallToolRequestParam, service::ServiceExt, transport::TokioChildProcess};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::task::JoinSet;

mod support;

async fn wait_for_socket(socket: &Path) -> Result<()> {
    let mut retries = 0usize;
    while retries < 80 {
        if UnixStream::connect(socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        retries += 1;
    }
    anyhow::bail!("timeout waiting for MCP daemon socket to become connectable")
}

async fn spawn_isolated_server(
    bin: &Path,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>> {
    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("RUST_LOG", "warn");

    let transport = TokioChildProcess::new(cmd).context("spawn isolated MCP server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting isolated MCP server")??;
    Ok(service)
}

async fn spawn_shared_daemon(bin: &Path, socket: &Path) -> Result<tokio::process::Child> {
    let mut daemon_cmd = Command::new(bin);
    daemon_cmd.arg("daemon").arg("--socket").arg(socket);
    daemon_cmd.env_remove("CONTEXT_MODEL_DIR");
    daemon_cmd.env("CONTEXT_PROFILE", "quality");
    daemon_cmd.env("CONTEXT_EMBEDDING_MODE", "stub");
    daemon_cmd.env("CONTEXT_DISABLE_DAEMON", "1");
    daemon_cmd.env("CONTEXT_INDEX_CONCURRENCY", "1");
    daemon_cmd.env("RUST_LOG", "warn");
    daemon_cmd.stdin(std::process::Stdio::null());
    daemon_cmd.stdout(std::process::Stdio::null());
    daemon_cmd.stderr(std::process::Stdio::null());
    daemon_cmd.spawn().context("spawn shared MCP daemon")
}

async fn spawn_shared_proxy(
    bin: &Path,
    socket: &Path,
    cwd: &Path,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>> {
    let mut cmd = Command::new(bin);
    cmd.current_dir(cwd);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");
    cmd.env("CONTEXT_MCP_SOCKET", socket.to_string_lossy().to_string());
    cmd.env("CONTEXT_MCP_SHARED", "1");
    cmd.env("RUST_LOG", "warn");

    let transport = TokioChildProcess::new(cmd).context("spawn shared-backend MCP proxy")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting shared-backend MCP proxy")??;
    Ok(service)
}

#[tokio::test]
async fn concurrent_isolated_servers_index_same_project_without_corruption() -> Result<()> {
    let bin = support::locate_context_mcp_bin()?;

    let tmp = tempfile::tempdir().context("temp project dir")?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    let marker = "IndexWriteLockSmoke";
    std::fs::write(root.join("src/lib.rs"), format!("pub struct {marker};\n"))
        .context("write src/lib.rs")?;

    let s1 = spawn_isolated_server(&bin).await?;
    let s2 = spawn_isolated_server(&bin).await?;

    let args = serde_json::json!({
        "path": root.to_string_lossy(),
        "query": marker,
        "limit": 2,
        "max_chars": 4000,
        "response_mode": "facts",
        "auto_index": true,
        "auto_index_budget_ms": 5000,
    });

    let call = |svc: rmcp::service::RunningService<rmcp::RoleClient, ()>| {
        let args = args.clone();
        async move {
            let resp = tokio::time::timeout(
                Duration::from_secs(20),
                svc.call_tool(CallToolRequestParam {
                    name: "context_pack".into(),
                    arguments: args.as_object().cloned(),
                }),
            )
            .await
            .context("timeout calling context_pack")?
            .context("call context_pack")?;
            assert_ne!(resp.is_error, Some(true), "context_pack returned error");
            let text = resp
                .content
                .first()
                .and_then(|c| c.as_text())
                .map(|t| t.text.as_str())
                .context("missing text output")?;
            let expected_snippet = format!("pub struct {marker}");
            assert!(
                text.contains(&expected_snippet),
                "expected output to mention marker: {text}"
            );
            svc.cancel().await.context("shutdown mcp service")?;
            Ok::<(), anyhow::Error>(())
        }
    };

    let (r1, r2) = tokio::join!(call(s1), call(s2));
    r1?;
    r2?;

    // Ensure the index file is parseable after concurrent indexing attempts.
    let index_path = context_dir_for_project_root(root)
        .join("indexes")
        .join("bge-small")
        .join("index.json");
    let bytes = std::fs::read(&index_path).context("read index.json")?;
    let parsed: Value = serde_json::from_slice(&bytes).context("parse index.json")?;
    assert!(
        parsed.get("schema_version").is_some(),
        "expected schema_version in index.json"
    );
    Ok(())
}

#[tokio::test]
async fn shared_backend_many_projects_concurrent_context_pack_is_stable() -> Result<()> {
    let bin = support::locate_context_mcp_bin()?;

    let socket_dir = tempfile::tempdir().context("tempdir for mcp daemon socket")?;
    let socket = socket_dir.path().join("mcp.sock");
    let mut daemon = spawn_shared_daemon(&bin, &socket).await?;

    let outcome = async {
        wait_for_socket(&socket).await?;

        let mut projects = Vec::new();
        for idx in 0..4usize {
            let tmp = tempfile::tempdir().context("temp project dir")?;
            let root = tmp.path().to_path_buf();
            std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
            std::fs::write(
                root.join("src/lib.rs"),
                format!("pub struct ProjectMarker{idx};\n"),
            )
            .context("write src/lib.rs")?;
            projects.push((tmp, root));
        }

        let mut join = JoinSet::new();
        for (idx, (_, root)) in projects.iter().enumerate() {
            let root = root.clone();
            let marker = format!("ProjectMarker{idx}");
            let svc = spawn_shared_proxy(&bin, &socket, &root).await?;
            join.spawn(async move {
                let args = serde_json::json!({
                    "query": marker,
                    "limit": 2,
                    "max_chars": 4000,
                    "response_mode": "facts",
                    "auto_index": true,
                    "auto_index_budget_ms": 5000,
                });
                let resp = tokio::time::timeout(
                    Duration::from_secs(25),
                    svc.call_tool(CallToolRequestParam {
                        name: "context_pack".into(),
                        arguments: args.as_object().cloned(),
                    }),
                )
                .await
                .context("timeout calling context_pack via shared backend")?
                .context("call context_pack via shared backend")?;
                assert_ne!(resp.is_error, Some(true), "context_pack returned error");
                let text = resp
                    .content
                    .first()
                    .and_then(|c| c.as_text())
                    .map(|t| t.text.as_str())
                    .context("missing text output")?;
                let expected_snippet = format!("pub struct {marker}");
                assert!(
                    text.contains(&expected_snippet),
                    "expected output to mention marker: {text}"
                );
                svc.cancel().await.context("shutdown proxy service")?;
                Ok::<(), anyhow::Error>(())
            });
        }

        while let Some(res) = join.join_next().await {
            res.context("join task")??;
        }

        Ok::<(), anyhow::Error>(())
    }
    .await;

    daemon.start_kill().ok();
    let _ = daemon.wait().await;

    outcome
}
