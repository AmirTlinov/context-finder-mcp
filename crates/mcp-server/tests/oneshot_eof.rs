use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
async fn oneshot_stdin_close_still_returns_response() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());

    let mut child = cmd.spawn().context("spawn mcp server")?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let stdout = child.stdout.take().context("stdout")?;
    let mut stdout = BufReader::new(stdout);

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "help", "arguments": {} }
    });
    send_line(&mut stdin, &req).await?;
    drop(stdin);

    let msg = read_line_json(&mut stdout).await?;
    assert_eq!(msg.get("id").and_then(Value::as_i64), Some(1));
    assert!(
        msg.get("result").is_some(),
        "expected tool response result in {msg:?}"
    );

    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .context("timeout waiting for mcp process to exit")??;
    assert!(status.success(), "mcp process exited with {status}");
    Ok(())
}
