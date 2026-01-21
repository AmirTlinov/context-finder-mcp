use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;

fn locate_context_finder_mcp_bin() -> Result<PathBuf> {
    if let Some(path) = option_env!("CARGO_BIN_EXE_context-finder-mcp") {
        return Ok(PathBuf::from(path));
    }

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

async fn wait_for_socket(socket: &std::path::Path) -> Result<()> {
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

async fn send_frame_with_content_type_first(
    stdin: &mut tokio::process::ChildStdin,
    value: &Value,
) -> Result<()> {
    let json = serde_json::to_vec(value)?;
    let header = format!(
        "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
        json.len()
    );
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

#[tokio::test]
async fn mcp_supports_content_length_framing() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env_remove("CONTEXT_FINDER_DAEMON_EXE");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());

    let mut child = cmd.spawn().context("spawn mcp server")?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let stdout = child.stdout.take().context("stdout")?;
    let mut stdout = BufReader::new(stdout);

    // initialize
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "content-length-smoke", "version": "0.1" }
        }
    });
    send_frame(&mut stdin, &init_req).await?;
    let init_resp = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut stdout))
        .await
        .context("timeout reading initialize response")??;
    assert_eq!(init_resp.get("id").and_then(Value::as_i64), Some(1));

    // initialized notification
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    send_frame(&mut stdin, &initialized).await?;

    // tools/list
    let list_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    send_frame(&mut stdin, &list_req).await?;
    let list_resp = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut stdout))
        .await
        .context("timeout reading tools/list response")??;
    assert_eq!(list_resp.get("id").and_then(Value::as_i64), Some(2));
    let tools = list_resp
        .get("result")
        .and_then(|v| v.get("tools"))
        .and_then(Value::as_array)
        .context("missing result.tools")?;
    assert!(
        tools
            .iter()
            .any(|t| t.get("name").and_then(Value::as_str) == Some("tree")),
        "tools/list missing 'tree'"
    );
    assert!(
        tools
            .iter()
            .any(|t| t.get("name").and_then(Value::as_str) == Some("text_search")),
        "tools/list missing 'text_search'"
    );

    // shutdown
    let _ = child.kill().await;
    Ok(())
}

#[tokio::test]
async fn mcp_accepts_content_type_before_content_length() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env_remove("CONTEXT_FINDER_DAEMON_EXE");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());

    let mut child = cmd.spawn().context("spawn mcp server")?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let stdout = child.stdout.take().context("stdout")?;
    let mut stdout = BufReader::new(stdout);

    // initialize (Content-Type header first is common in JSON-RPC/LSP clients)
    let init_req = serde_json::json!( {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "content-type-smoke", "version": "0.1" }
        }
    });
    send_frame_with_content_type_first(&mut stdin, &init_req).await?;
    let init_resp = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut stdout))
        .await
        .context("timeout reading initialize response")??;
    assert_eq!(init_resp.get("id").and_then(Value::as_i64), Some(1));

    // shutdown
    let _ = child.kill().await;
    Ok(())
}

#[tokio::test]
async fn mcp_initialize_does_not_block_on_roots_list() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env_remove("CONTEXT_FINDER_DAEMON_EXE");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());

    let mut child = cmd.spawn().context("spawn mcp server")?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let stdout = child.stdout.take().context("stdout")?;
    let mut stdout = BufReader::new(stdout);

    // initialize (with roots capability)
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": { "roots": {} },
            "clientInfo": { "name": "roots-init-smoke", "version": "0.1" }
        }
    });
    send_frame(&mut stdin, &init_req).await?;

    // The server should reply immediately; roots/list must never block initialization.
    let init_resp = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let frame = read_frame(&mut stdout).await?;
            if frame.get("id").and_then(Value::as_i64) == Some(1) {
                return Ok::<_, anyhow::Error>(frame);
            }
        }
    })
    .await
    .context("timeout reading initialize response")??;
    assert_eq!(init_resp.get("id").and_then(Value::as_i64), Some(1));

    // shutdown
    let _ = child.kill().await;
    Ok(())
}

#[tokio::test]
async fn mcp_accepts_tools_list_without_initialized_notification() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env_remove("CONTEXT_FINDER_DAEMON_EXE");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());

    let mut child = cmd.spawn().context("spawn mcp server")?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let stdout = child.stdout.take().context("stdout")?;
    let mut stdout = BufReader::new(stdout);

    // initialize
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "content-length-no-init", "version": "0.1" }
        }
    });
    send_frame(&mut stdin, &init_req).await?;
    let init_resp = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut stdout))
        .await
        .context("timeout reading initialize response")??;
    assert_eq!(init_resp.get("id").and_then(Value::as_i64), Some(1));

    // IMPORTANT: deliberately skip `notifications/initialized` and go straight to tools/list.
    let list_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    send_frame(&mut stdin, &list_req).await?;
    let list_resp = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut stdout))
        .await
        .context("timeout reading tools/list response")??;
    assert_eq!(list_resp.get("id").and_then(Value::as_i64), Some(2));

    let tools = list_resp
        .get("result")
        .and_then(|v| v.get("tools"))
        .and_then(Value::as_array)
        .context("missing result.tools")?;
    assert!(
        tools
            .iter()
            .any(|t| t.get("name").and_then(Value::as_str) == Some("read_pack")),
        "tools/list missing 'read_pack'"
    );

    let _ = child.kill().await;
    Ok(())
}

#[tokio::test]
async fn mcp_accepts_tools_call_without_mcp_handshake() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env_remove("CONTEXT_FINDER_DAEMON_EXE");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());

    let mut child = cmd.spawn().context("spawn mcp server")?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let stdout = child.stdout.take().context("stdout")?;
    let mut stdout = BufReader::new(stdout);

    // Tool runners may skip MCP handshake and call tools directly.
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "capabilities",
            "arguments": {}
        }
    });
    send_frame(&mut stdin, &call).await?;
    let resp = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut stdout))
        .await
        .context("timeout reading tools/call response")??;
    assert_eq!(resp.get("id").and_then(Value::as_i64), Some(1));

    let _ = child.kill().await;
    Ok(())
}

#[tokio::test]
async fn shared_backend_proxy_supports_content_length_and_path_injection() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let socket_dir = tempfile::tempdir().context("tempdir for mcp daemon socket")?;
    let socket = socket_dir.path().join("mcp.sock");

    let mut daemon_cmd = Command::new(&bin);
    daemon_cmd.arg("daemon").arg("--socket").arg(&socket);
    daemon_cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    daemon_cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    daemon_cmd.env_remove("CONTEXT_FINDER_DAEMON_EXE");
    daemon_cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    daemon_cmd.env("RUST_LOG", "warn");
    daemon_cmd.stdin(std::process::Stdio::null());
    daemon_cmd.stdout(std::process::Stdio::null());
    daemon_cmd.stderr(std::process::Stdio::null());
    let mut daemon = daemon_cmd.spawn().context("spawn mcp daemon")?;

    let outcome = async {
        wait_for_socket(&socket).await?;

        // Proxy process (shared mode) with content-length client framing.
        let tmp = tempfile::tempdir().context("temp project dir")?;
        let root = tmp.path();
        std::fs::write(root.join("README.md"), "hello\nworld\n").context("write README.md")?;

        let mut cmd = Command::new(bin);
        cmd.current_dir(root);
        cmd.env("CONTEXT_FINDER_PROFILE", "quality");
        cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
        cmd.env_remove("CONTEXT_FINDER_DAEMON_EXE");
        cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
        cmd.env("RUST_LOG", "warn");
        cmd.env("CONTEXT_FINDER_MCP_SHARED", "1");
        cmd.env(
            "CONTEXT_FINDER_MCP_SOCKET",
            socket.to_string_lossy().to_string(),
        );
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());

        let mut child = cmd.spawn().context("spawn shared-backend mcp proxy")?;
        let mut stdin = child.stdin.take().context("stdin")?;
        let stdout = child.stdout.take().context("stdout")?;
        let mut stdout = BufReader::new(stdout);

        // initialize
        let init_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "content-length-shared-smoke", "version": "0.1" }
            }
        });
        send_frame(&mut stdin, &init_req).await?;
        let init_resp = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut stdout))
            .await
            .context("timeout reading initialize response")??;
        assert_eq!(init_resp.get("id").and_then(Value::as_i64), Some(1));

        // initialized notification
        let initialized = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        send_frame(&mut stdin, &initialized).await?;

        // tools/call: cat without path (proxy should inject from cwd)
        let call_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "cat",
                "arguments": {
                    "file": "README.md",
                    "max_lines": 1,
                    "max_chars": 2048
                }
            }
        });
        send_frame(&mut stdin, &call_req).await?;
        let call_resp = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut stdout))
            .await
            .context("timeout reading tools/call response")??;
        assert_eq!(call_resp.get("id").and_then(Value::as_i64), Some(2));

        let text = call_resp
            .get("result")
            .and_then(|v| v.get("content"))
            .and_then(Value::as_array)
            .and_then(|items| {
                items.iter().find_map(|item| {
                    if item.get("type").and_then(Value::as_str) != Some("text") {
                        return None;
                    }
                    item.get("text").and_then(Value::as_str)
                })
            })
            .context("missing result.content text block")?;
        assert!(
            text.contains("hello"),
            "expected file_slice output to include the first line"
        );

        let _ = child.kill().await;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    daemon.start_kill().ok();
    let _ = daemon.wait().await;

    outcome
}

#[tokio::test]
async fn shared_backend_proxy_synthesizes_initialized_notification_if_client_skips_it() -> Result<()>
{
    let bin = locate_context_finder_mcp_bin()?;

    let socket_dir = tempfile::tempdir().context("tempdir for mcp daemon socket")?;
    let socket = socket_dir.path().join("mcp.sock");

    let mut daemon_cmd = Command::new(&bin);
    daemon_cmd.arg("daemon").arg("--socket").arg(&socket);
    daemon_cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    daemon_cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    daemon_cmd.env_remove("CONTEXT_FINDER_DAEMON_EXE");
    daemon_cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    daemon_cmd.env("RUST_LOG", "warn");
    daemon_cmd.stdin(std::process::Stdio::null());
    daemon_cmd.stdout(std::process::Stdio::null());
    daemon_cmd.stderr(std::process::Stdio::null());
    let mut daemon = daemon_cmd.spawn().context("spawn mcp daemon")?;

    let outcome = async {
        wait_for_socket(&socket).await?;

        let mut cmd = Command::new(bin);
        cmd.env("CONTEXT_FINDER_PROFILE", "quality");
        cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
        cmd.env_remove("CONTEXT_FINDER_DAEMON_EXE");
        cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
        cmd.env("RUST_LOG", "warn");
        cmd.env("CONTEXT_FINDER_MCP_SHARED", "1");
        cmd.env(
            "CONTEXT_FINDER_MCP_SOCKET",
            socket.to_string_lossy().to_string(),
        );
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());

        let mut child = cmd.spawn().context("spawn shared-backend mcp proxy")?;
        let mut stdin = child.stdin.take().context("stdin")?;
        let stdout = child.stdout.take().context("stdout")?;
        let mut stdout = BufReader::new(stdout);

        // initialize
        let init_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "content-length-shared-no-init", "version": "0.1" }
            }
        });
        send_frame(&mut stdin, &init_req).await?;
        let init_resp = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut stdout))
            .await
            .context("timeout reading initialize response")??;
        assert_eq!(init_resp.get("id").and_then(Value::as_i64), Some(1));

        // IMPORTANT: deliberately skip `notifications/initialized` and go straight to tools/list.
        let list_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        });
        send_frame(&mut stdin, &list_req).await?;
        let list_resp = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut stdout))
            .await
            .context("timeout reading tools/list response")??;
        assert_eq!(list_resp.get("id").and_then(Value::as_i64), Some(2));

        let tools = list_resp
            .get("result")
            .and_then(|v| v.get("tools"))
            .and_then(Value::as_array)
            .context("missing result.tools")?;
        assert!(
            tools
                .iter()
                .any(|t| t.get("name").and_then(Value::as_str) == Some("read_pack")),
            "tools/list missing 'read_pack'"
        );

        let _ = child.kill().await;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    daemon.start_kill().ok();
    let _ = daemon.wait().await;

    outcome
}
