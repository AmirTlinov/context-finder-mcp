use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

mod support;

async fn send_line(stdin: &mut tokio::process::ChildStdin, value: &Value) -> Result<()> {
    let mut json = serde_json::to_vec(value)?;
    json.push(b'\n');
    stdin.write_all(&json).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_line_json(stdout: &mut BufReader<tokio::process::ChildStdout>) -> Result<Value> {
    loop {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(10), stdout.read_line(&mut line))
            .await
            .context("timeout reading json line")??;
        if n == 0 {
            anyhow::bail!("EOF while reading json line");
        }
        if line.trim().is_empty() {
            continue;
        }
        return Ok(serde_json::from_str(&line)?);
    }
}

#[tokio::test]
async fn mcp_accepts_newline_json_batch_messages() -> Result<()> {
    let bin = support::locate_context_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());

    let mut child = cmd.spawn().context("spawn mcp server")?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let stdout = child.stdout.take().context("stdout")?;
    let mut stdout = BufReader::new(stdout);

    // Send a single newline-delimited JSON batch: initialize + initialized + tools/list.
    let batch = serde_json::json!([
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "newline-batch-smoke", "version": "0.1" }
            }
        },
        {
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        },
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }
    ]);
    send_line(&mut stdin, &batch).await?;

    let mut init_seen = false;
    let mut list_seen = false;
    for _ in 0..10 {
        let msg = read_line_json(&mut stdout).await?;
        if msg.get("id").and_then(Value::as_i64) == Some(1) {
            init_seen = true;
        }
        if msg.get("id").and_then(Value::as_i64) == Some(2) {
            list_seen = true;
            let tools = msg
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
            break;
        }
    }

    assert!(init_seen, "did not observe initialize response");
    assert!(list_seen, "did not observe tools/list response");

    let _ = child.kill().await;
    Ok(())
}
