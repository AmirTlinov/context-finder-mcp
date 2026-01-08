use anyhow::{Context, Result};
use rmcp::{model::CallToolRequestParam, service::ServiceExt, transport::TokioChildProcess};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
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

async fn wait_for_socket(socket: &Path) -> Result<()> {
    let mut retries = 0usize;
    while retries < 40 {
        if UnixStream::connect(socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        retries += 1;
    }
    anyhow::bail!("timeout waiting for MCP daemon socket to become connectable")
}

async fn send_frame(stdin: &mut tokio::process::ChildStdin, value: &Value) -> Result<()> {
    let json = serde_json::to_vec(value)?;
    let header = format!("Content-Length: {}\r\n\r\n", json.len());
    stdin.write_all(header.as_bytes()).await?;
    stdin.write_all(&json).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_frame(stdout: &mut BufReader<tokio::process::ChildStdout>) -> Result<Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = stdout.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("EOF while reading MCP frame headers");
        }
        if line == "\n" || line == "\r\n" {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            content_length = Some(rest.trim().parse::<usize>()?);
        }
    }
    let len = content_length.context("missing Content-Length header")?;

    let mut body = vec![0u8; len];
    stdout.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

async fn spawn_shared_proxy(
    bin: &Path,
    socket: &Path,
    cwd: Option<&Path>,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>> {
    let mut cmd = Command::new(bin);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    cmd.env(
        "CONTEXT_FINDER_MCP_SOCKET",
        socket.to_string_lossy().to_string(),
    );
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting shared-backend MCP proxy")??;
    Ok(service)
}

async fn spawn_shared_proxy_allow_daemon_spawn(
    bin: &Path,
    socket: &Path,
    cwd: Option<&Path>,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>> {
    let mut cmd = Command::new(bin);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env(
        "CONTEXT_FINDER_MCP_SOCKET",
        socket.to_string_lossy().to_string(),
    );
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting shared-backend MCP proxy")??;
    Ok(service)
}

#[tokio::test]
async fn shared_backend_proxy_roundtrips_tool_calls() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let socket_dir = tempfile::tempdir().context("tempdir for mcp daemon socket")?;
    let socket = socket_dir.path().join("mcp.sock");

    let mut daemon_cmd = Command::new(&bin);
    daemon_cmd.arg("daemon").arg("--socket").arg(&socket);
    daemon_cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    daemon_cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    daemon_cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    daemon_cmd.stdin(std::process::Stdio::null());
    daemon_cmd.stdout(std::process::Stdio::null());
    daemon_cmd.stderr(std::process::Stdio::null());
    let mut daemon = daemon_cmd.spawn().context("spawn mcp daemon")?;

    let outcome = async {
        wait_for_socket(&socket).await?;

        let service = spawn_shared_proxy(&bin, &socket, None).await?;

        let tmp = tempfile::tempdir().context("temp project dir")?;
        let root = tmp.path();
        std::fs::write(root.join("README.md"), "hello\nworld\n").context("write README.md")?;

        let args = serde_json::json!({
            "path": root.to_string_lossy(),
            "file": "README.md",
            "max_lines": 1,
            "max_chars": 2048,
        });
        let resp = tokio::time::timeout(
            Duration::from_secs(10),
            service.call_tool(CallToolRequestParam {
                name: "file_slice".into(),
                arguments: args.as_object().cloned(),
            }),
        )
        .await
        .context("timeout calling file_slice via shared backend proxy")??;

        let text = resp
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .context("missing file_slice text content")?;
        assert!(text.contains("README.md"));
        assert!(text.contains("\nhello\n"));
        assert!(!text.contains("world"));

        Ok::<(), anyhow::Error>(())
    }
    .await;

    // Ensure we don't leak a long-lived daemon process after the test.
    daemon.start_kill().ok();
    let _ = daemon.wait().await;

    outcome
}

#[tokio::test]
async fn shared_backend_proxy_accepts_tools_call_without_handshake() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let socket_dir = tempfile::tempdir().context("tempdir for mcp daemon socket")?;
    let socket = socket_dir.path().join("mcp.sock");

    let mut daemon_cmd = Command::new(&bin);
    daemon_cmd.arg("daemon").arg("--socket").arg(&socket);
    daemon_cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    daemon_cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    daemon_cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    daemon_cmd.stdin(std::process::Stdio::null());
    daemon_cmd.stdout(std::process::Stdio::null());
    daemon_cmd.stderr(std::process::Stdio::null());
    let mut daemon = daemon_cmd.spawn().context("spawn mcp daemon")?;

    let outcome = async {
        wait_for_socket(&socket).await?;

        let tmp = tempfile::tempdir().context("temp project dir")?;
        let root = tmp.path();
        std::fs::write(root.join("README.md"), "hello\nworld\n").context("write README.md")?;

        let mut proxy_cmd = Command::new(&bin);
        proxy_cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
        proxy_cmd.env("CONTEXT_FINDER_PROFILE", "quality");
        proxy_cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
        proxy_cmd.env(
            "CONTEXT_FINDER_MCP_SOCKET",
            socket.to_string_lossy().to_string(),
        );
        proxy_cmd.env("CONTEXT_FINDER_MCP_SHARED", "1");
        proxy_cmd.stdin(std::process::Stdio::piped());
        proxy_cmd.stdout(std::process::Stdio::piped());
        proxy_cmd.stderr(std::process::Stdio::null());

        let mut proxy = proxy_cmd.spawn().context("spawn shared proxy")?;
        let mut stdin = proxy.stdin.take().context("proxy stdin")?;
        let stdout = proxy.stdout.take().context("proxy stdout")?;
        let mut stdout = BufReader::new(stdout);

        // Tool runners may skip MCP handshake and call tools directly.
        let call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "file_slice",
                "arguments": {
                    "path": root.to_string_lossy(),
                    "file": "README.md",
                    "max_lines": 1,
                    "max_chars": 2048,
                }
            }
        });
        send_frame(&mut stdin, &call).await?;
        let resp = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut stdout))
            .await
            .context("timeout reading tools/call response")??;
        assert_eq!(resp.get("id").and_then(Value::as_i64), Some(1));

        let text = resp
            .get("result")
            .and_then(|v| v.get("content"))
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str)
            .context("missing response result.content[0].text")?;
        assert!(text.contains("\nhello\n"));
        assert!(!text.contains("world"));

        proxy.start_kill().ok();
        let _ = proxy.wait().await;

        Ok::<(), anyhow::Error>(())
    }
    .await;

    daemon.start_kill().ok();
    let _ = daemon.wait().await;

    outcome
}

#[tokio::test]
async fn shared_backend_daemon_recovers_from_stale_socket_file() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let socket_dir = tempfile::tempdir().context("tempdir for mcp daemon socket")?;
    let socket = socket_dir.path().join("mcp.sock");

    // Simulate a stale socket file: bind a listener to create the socket path,
    // then drop it without removing the file.
    let listener = tokio::net::UnixListener::bind(&socket).context("bind temp unix listener")?;
    drop(listener);

    let mut daemon_cmd = Command::new(&bin);
    daemon_cmd.arg("daemon").arg("--socket").arg(&socket);
    daemon_cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    daemon_cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    daemon_cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    daemon_cmd.stdin(std::process::Stdio::null());
    daemon_cmd.stdout(std::process::Stdio::null());
    daemon_cmd.stderr(std::process::Stdio::null());
    let mut daemon = daemon_cmd.spawn().context("spawn mcp daemon")?;

    let outcome = async {
        wait_for_socket(&socket).await?;

        let service = spawn_shared_proxy(&bin, &socket, None).await?;

        let tmp = tempfile::tempdir().context("temp project dir")?;
        let root = tmp.path();
        std::fs::write(root.join("README.md"), "hello\nworld\n").context("write README.md")?;

        let args = serde_json::json!({
            "path": root.to_string_lossy(),
            "file": "README.md",
            "max_lines": 1,
            "max_chars": 2048,
        });
        let resp = tokio::time::timeout(
            Duration::from_secs(10),
            service.call_tool(CallToolRequestParam {
                name: "file_slice".into(),
                arguments: args.as_object().cloned(),
            }),
        )
        .await
        .context("timeout calling file_slice via shared backend proxy")??;

        let text = resp
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .context("missing file_slice text content")?;
        assert!(text.contains("\nhello\n"));
        assert!(!text.contains("world"));

        service.cancel().await.context("shutdown proxy service")?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    daemon.start_kill().ok();
    let _ = daemon.wait().await;

    outcome
}

#[tokio::test]
async fn shared_backend_proxy_injects_path_from_cwd_on_first_call() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let socket_dir = tempfile::tempdir().context("tempdir for mcp daemon socket")?;
    let socket = socket_dir.path().join("mcp.sock");

    let mut daemon_cmd = Command::new(&bin);
    daemon_cmd.arg("daemon").arg("--socket").arg(&socket);
    daemon_cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    daemon_cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    daemon_cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    daemon_cmd.stdin(std::process::Stdio::null());
    daemon_cmd.stdout(std::process::Stdio::null());
    daemon_cmd.stderr(std::process::Stdio::null());
    let mut daemon = daemon_cmd.spawn().context("spawn mcp daemon")?;

    let outcome = async {
        wait_for_socket(&socket).await?;

        let tmp = tempfile::tempdir().context("temp project dir")?;
        let root = tmp.path();
        std::fs::write(root.join("README.md"), "hello\nworld\n").context("write README.md")?;

        // Start the proxy with cwd=root and omit path in the first tool call.
        let service = spawn_shared_proxy(&bin, &socket, Some(root)).await?;

        let args_no_path = serde_json::json!({
            "file": "README.md",
            "max_lines": 1,
            "max_chars": 2048,
        });

        let resp = tokio::time::timeout(
            Duration::from_secs(10),
            service.call_tool(CallToolRequestParam {
                name: "file_slice".into(),
                arguments: args_no_path.as_object().cloned(),
            }),
        )
        .await
        .context("timeout calling file_slice (no path) via shared backend proxy")??;

        let text = resp
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .context("missing file_slice text content")?;
        assert!(text.contains("\nhello\n"));
        assert!(!text.contains("world"));

        Ok::<(), anyhow::Error>(())
    }
    .await;

    daemon.start_kill().ok();
    let _ = daemon.wait().await;

    outcome
}

#[tokio::test]
async fn shared_backend_proxy_recovers_when_daemon_restarts() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let socket_dir = tempfile::tempdir().context("tempdir for mcp daemon socket")?;
    let socket = socket_dir.path().join("mcp.sock");

    let mut daemon_cmd = Command::new(&bin);
    daemon_cmd.arg("daemon").arg("--socket").arg(&socket);
    daemon_cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    daemon_cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    daemon_cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    daemon_cmd.stdin(std::process::Stdio::null());
    daemon_cmd.stdout(std::process::Stdio::null());
    daemon_cmd.stderr(std::process::Stdio::null());
    let mut daemon = daemon_cmd.spawn().context("spawn mcp daemon")?;

    let outcome = async {
        wait_for_socket(&socket).await?;

        let service = spawn_shared_proxy(&bin, &socket, None).await?;

        let tmp = tempfile::tempdir().context("temp project dir")?;
        let root = tmp.path();
        std::fs::write(root.join("README.md"), "hello\nworld\n").context("write README.md")?;

        let args = serde_json::json!({
            "path": root.to_string_lossy(),
            "file": "README.md",
            "max_lines": 1,
            "max_chars": 2048,
        });

        // First call uses the initial daemon.
        let resp1 = tokio::time::timeout(
            Duration::from_secs(10),
            service.call_tool(CallToolRequestParam {
                name: "file_slice".into(),
                arguments: args.as_object().cloned(),
            }),
        )
        .await
        .context("timeout calling file_slice (first)")??;
        let text1 = resp1
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .context("missing file_slice (first) text content")?;
        assert!(text1.contains("\nhello\n"));
        assert!(!text1.contains("world"));

        // Kill the daemon and bring it back.
        daemon.start_kill().ok();
        let _ = daemon.wait().await;

        let mut daemon2_cmd = Command::new(&bin);
        daemon2_cmd.arg("daemon").arg("--socket").arg(&socket);
        daemon2_cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
        daemon2_cmd.env("CONTEXT_FINDER_PROFILE", "quality");
        daemon2_cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
        daemon2_cmd.stdin(std::process::Stdio::null());
        daemon2_cmd.stdout(std::process::Stdio::null());
        daemon2_cmd.stderr(std::process::Stdio::null());
        let mut daemon2 = daemon2_cmd.spawn().context("spawn mcp daemon (restart)")?;
        wait_for_socket(&socket).await?;

        // Second call should succeed without restarting the proxy.
        let resp2 = tokio::time::timeout(
            Duration::from_secs(10),
            service.call_tool(CallToolRequestParam {
                name: "file_slice".into(),
                arguments: args.as_object().cloned(),
            }),
        )
        .await
        .context("timeout calling file_slice (after restart)")??;
        let text2 = resp2
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .context("missing file_slice (after restart) text content")?;
        assert!(text2.contains("\nhello\n"));
        assert!(!text2.contains("world"));

        daemon2.start_kill().ok();
        let _ = daemon2.wait().await;

        service.cancel().await.context("shutdown proxy service")?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    // Ensure we don't leak a long-lived daemon process after the test.
    daemon.start_kill().ok();
    let _ = daemon.wait().await;

    outcome
}

#[tokio::test]
async fn shared_backend_proxy_restarts_daemon_when_binary_changes() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let socket_dir = tempfile::tempdir().context("tempdir for mcp daemon socket")?;
    let socket = socket_dir.path().join("mcp.sock");
    let pid_path = socket.with_extension("pid");

    let mut daemon_cmd = Command::new(&bin);
    daemon_cmd.arg("daemon").arg("--socket").arg(&socket);
    daemon_cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    daemon_cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    daemon_cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    daemon_cmd.stdin(std::process::Stdio::null());
    daemon_cmd.stdout(std::process::Stdio::null());
    daemon_cmd.stderr(std::process::Stdio::null());
    let mut daemon = daemon_cmd.spawn().context("spawn mcp daemon")?;

    let outcome = async {
        wait_for_socket(&socket).await?;

        let pid_bytes = tokio::fs::read(&pid_path)
            .await
            .with_context(|| format!("read mcp daemon pid file at {}", pid_path.display()))?;
        let mut pid_json: Value =
            serde_json::from_slice(&pid_bytes).context("parse mcp daemon pid JSON")?;
        let old_pid = pid_json
            .get("pid")
            .and_then(Value::as_u64)
            .context("pid file missing pid")?;

        // Simulate "binary updated after daemon start": force an old started_at_ms so the proxy
        // restarts the daemon even if it's responsive.
        if let Some(obj) = pid_json.as_object_mut() {
            obj.insert("started_at_ms".to_string(), Value::Number(1u64.into()));
        }
        tokio::fs::write(&pid_path, serde_json::to_vec(&pid_json)?)
            .await
            .context("rewrite daemon pid file with stale started_at_ms")?;

        let service = spawn_shared_proxy_allow_daemon_spawn(&bin, &socket, None).await?;

        let tmp = tempfile::tempdir().context("temp project dir")?;
        let root = tmp.path();
        std::fs::write(root.join("README.md"), "hello\nworld\n").context("write README.md")?;

        let args = serde_json::json!({
            "path": root.to_string_lossy(),
            "file": "README.md",
            "max_lines": 1,
            "max_chars": 2048,
        });
        let resp = tokio::time::timeout(
            Duration::from_secs(10),
            service.call_tool(CallToolRequestParam {
                name: "file_slice".into(),
                arguments: args.as_object().cloned(),
            }),
        )
        .await
        .context("timeout calling file_slice via restarted daemon")??;
        let text = resp
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .context("missing file_slice text content")?;
        assert!(text.contains("\nhello\n"));
        assert!(!text.contains("world"));

        // Confirm the daemon was restarted (pid should change).
        let new_pid_bytes = tokio::fs::read(&pid_path).await.with_context(|| {
            format!("read updated mcp daemon pid file at {}", pid_path.display())
        })?;
        let new_pid_json: Value =
            serde_json::from_slice(&new_pid_bytes).context("parse updated pid JSON")?;
        let new_pid = new_pid_json
            .get("pid")
            .and_then(Value::as_u64)
            .context("updated pid file missing pid")?;
        assert_ne!(
            old_pid, new_pid,
            "expected proxy to restart daemon due to stale pid metadata"
        );

        service.cancel().await.context("shutdown proxy service")?;

        // Orphan the daemon by removing its socket; it should exit cleanly.
        tokio::fs::remove_file(&socket).await.ok();

        Ok::<(), anyhow::Error>(())
    }
    .await;

    daemon.start_kill().ok();
    let _ = daemon.wait().await;

    outcome
}

#[tokio::test]
async fn shared_backend_sessions_do_not_share_default_root() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let socket_dir = tempfile::tempdir().context("tempdir for mcp daemon socket")?;
    let socket = socket_dir.path().join("mcp.sock");

    let mut daemon_cmd = Command::new(&bin);
    daemon_cmd.arg("daemon").arg("--socket").arg(&socket);
    daemon_cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    daemon_cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    daemon_cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    daemon_cmd.stdin(std::process::Stdio::null());
    daemon_cmd.stdout(std::process::Stdio::null());
    daemon_cmd.stderr(std::process::Stdio::null());
    let mut daemon = daemon_cmd.spawn().context("spawn mcp daemon")?;

    let outcome = async {
        wait_for_socket(&socket).await?;

        let service_a = spawn_shared_proxy(&bin, &socket, None).await?;
        let service_b = spawn_shared_proxy(&bin, &socket, None).await?;

        let tmp_a = tempfile::tempdir().context("temp project dir A")?;
        let root_a = tmp_a.path();
        std::fs::write(root_a.join("README.md"), "alpha\n").context("write README A")?;

        let tmp_b = tempfile::tempdir().context("temp project dir B")?;
        let root_b = tmp_b.path();
        std::fs::write(root_b.join("README.md"), "beta\n").context("write README B")?;

        let args_a = serde_json::json!({
            "path": root_a.to_string_lossy(),
            "file": "README.md",
            "max_lines": 1,
            "max_chars": 2048,
        });
        let args_b = serde_json::json!({
            "path": root_b.to_string_lossy(),
            "file": "README.md",
            "max_lines": 1,
            "max_chars": 2048,
        });

        let resp_a1 = tokio::time::timeout(
            Duration::from_secs(10),
            service_a.call_tool(CallToolRequestParam {
                name: "file_slice".into(),
                arguments: args_a.as_object().cloned(),
            }),
        )
        .await
        .context("timeout calling file_slice A (with path)")??;
        let text_a1 = resp_a1
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .context("missing file_slice A text content")?;
        assert!(text_a1.contains("\nalpha\n"));

        let resp_b1 = tokio::time::timeout(
            Duration::from_secs(10),
            service_b.call_tool(CallToolRequestParam {
                name: "file_slice".into(),
                arguments: args_b.as_object().cloned(),
            }),
        )
        .await
        .context("timeout calling file_slice B (with path)")??;
        let text_b1 = resp_b1
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .context("missing file_slice B text content")?;
        assert!(text_b1.contains("\nbeta\n"));

        // Now omit `path` and rely on per-connection session defaults. If the daemon shares session
        // defaults across connections, this will non-deterministically read the wrong project.
        let args_no_path = serde_json::json!({
            "file": "README.md",
            "max_lines": 1,
            "max_chars": 2048,
        });

        let resp_a2 = tokio::time::timeout(
            Duration::from_secs(10),
            service_a.call_tool(CallToolRequestParam {
                name: "file_slice".into(),
                arguments: args_no_path.as_object().cloned(),
            }),
        )
        .await
        .context("timeout calling file_slice A (no path)")??;
        let text_a2 = resp_a2
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .context("missing file_slice A (no path) text content")?;
        assert!(text_a2.contains("\nalpha\n"));

        let resp_b2 = tokio::time::timeout(
            Duration::from_secs(10),
            service_b.call_tool(CallToolRequestParam {
                name: "file_slice".into(),
                arguments: args_no_path.as_object().cloned(),
            }),
        )
        .await
        .context("timeout calling file_slice B (no path)")??;
        let text_b2 = resp_b2
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .context("missing file_slice B (no path) text content")?;
        assert!(text_b2.contains("\nbeta\n"));

        Ok::<(), anyhow::Error>(())
    }
    .await;

    daemon.start_kill().ok();
    let _ = daemon.wait().await;

    outcome
}

#[tokio::test]
async fn shared_backend_proxy_recovers_from_unresponsive_daemon() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let socket_dir = tempfile::tempdir().context("tempdir for mcp daemon socket")?;
    let socket = socket_dir.path().join("mcp.sock");
    let pid_path = socket.with_extension("pid");

    // Simulate a connectable-but-unresponsive daemon (e.g., SIGSTOP'ed process):
    // accept the connection and then never respond.
    let listener = tokio::net::UnixListener::bind(&socket).context("bind dummy daemon socket")?;
    let dummy = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(stream);
        }
    });

    let outcome = async {
        // The proxy should detect the unresponsive backend and self-heal by replacing it with a
        // real daemon.
        let service = spawn_shared_proxy_allow_daemon_spawn(&bin, &socket, None).await?;

        let tmp = tempfile::tempdir().context("temp project dir")?;
        let root = tmp.path();
        std::fs::write(root.join("README.md"), "hello\nworld\n").context("write README.md")?;

        let args = serde_json::json!({
            "path": root.to_string_lossy(),
            "file": "README.md",
            "max_lines": 1,
            "max_chars": 2048,
        });
        let resp = tokio::time::timeout(
            Duration::from_secs(10),
            service.call_tool(CallToolRequestParam {
                name: "file_slice".into(),
                arguments: args.as_object().cloned(),
            }),
        )
        .await
        .context("timeout calling file_slice after daemon recovery")??;

        let text = resp
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .context("missing file_slice text content")?;
        assert!(text.contains("\nhello\n"));
        assert!(!text.contains("world"));

        service.cancel().await.context("shutdown proxy service")?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    // Cleanup: stop the dummy listener task.
    dummy.abort();
    let _ = dummy.await;

    // Cleanup: if the proxy spawned a daemon, terminate it via pid file.
    if let Ok(bytes) = tokio::fs::read(&pid_path).await {
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(pid) = value.get("pid").and_then(Value::as_u64) {
                unsafe {
                    let _ = libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
    }

    outcome
}
