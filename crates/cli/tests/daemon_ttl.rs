#![allow(deprecated)]

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tempfile::tempdir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Debug, Deserialize)]
struct StatusResponse {
    status: String,
    #[allow(dead_code)]
    message: Option<String>,
    #[serde(default)]
    projects: Vec<StatusProject>,
}

#[derive(Debug, Deserialize)]
struct StatusProject {
    project: String,
    #[allow(dead_code)]
    age_ms: u64,
    #[allow(dead_code)]
    ttl_ms: u64,
}

async fn wait_for_socket(socket: &Path) -> Result<()> {
    let started = tokio::time::Instant::now();
    loop {
        if UnixStream::connect(socket).await.is_ok() {
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(2) {
            anyhow::bail!("daemon socket did not become ready: {}", socket.display());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn send_json(socket: &Path, payload: &serde_json::Value) -> Result<serde_json::Value> {
    let mut stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to {}", socket.display()))?;
    let msg = serde_json::to_string(payload)? + "\n";
    stream.write_all(msg.as_bytes()).await?;
    stream.flush().await?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    Ok(serde_json::from_str(&line)?)
}

async fn ping(socket: &Path, project: &Path, ttl_ms: u64) -> Result<()> {
    let payload = serde_json::json!({
        "cmd": "ping",
        "project": project.to_string_lossy(),
        "ttl_ms": ttl_ms,
    });
    let resp = send_json(socket, &payload).await?;
    anyhow::ensure!(resp["status"] == "ok", "ping failed: {resp}");
    Ok(())
}

async fn status(socket: &Path) -> Result<StatusResponse> {
    let payload = serde_json::json!({
        "cmd": "status",
        "project": "",
    });
    let resp = send_json(socket, &payload).await?;
    Ok(serde_json::from_value(resp)?)
}

fn make_stub_project() -> Result<tempfile::TempDir> {
    let dir = tempdir()?;
    std::fs::create_dir_all(dir.path().join("src"))?;
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn hello() {}")?;
    Ok(dir)
}

#[tokio::test]
async fn daemon_tracks_multiple_projects_and_expires() -> Result<()> {
    let sock_dir = tempdir()?;
    let socket = sock_dir.path().join("daemon.sock");

    let bin = assert_cmd::cargo::cargo_bin("context");
    let mut daemon = tokio::process::Command::new(bin)
        .arg("daemon-loop")
        .arg("--socket")
        .arg(&socket)
        .env("CONTEXT_EMBEDDING_MODE", "stub")
        .env("CONTEXT_DAEMON_TTL_MS", "2000")
        .env("CONTEXT_DAEMON_CLEANUP_MS", "25")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn daemon-loop")?;

    wait_for_socket(&socket).await?;

    let proj_a = make_stub_project()?;
    let proj_b = make_stub_project()?;

    ping(&socket, proj_a.path(), 150).await?;
    ping(&socket, proj_b.path(), 150).await?;

    let mut st = status(&socket).await?;
    anyhow::ensure!(st.status == "ok");
    st.projects.sort_by(|a, b| a.project.cmp(&b.project));
    anyhow::ensure!(st.projects.len() == 2);

    tokio::time::sleep(Duration::from_millis(250)).await;
    let st2 = status(&socket).await?;
    anyhow::ensure!(st2.projects.is_empty());

    let _ = daemon.kill().await;
    let _ = daemon.wait().await;
    Ok(())
}

#[tokio::test]
async fn ping_extends_project_ttl() -> Result<()> {
    let sock_dir = tempdir()?;
    let socket = sock_dir.path().join("daemon.sock");

    let bin = assert_cmd::cargo::cargo_bin("context");
    let mut daemon = tokio::process::Command::new(bin)
        .arg("daemon-loop")
        .arg("--socket")
        .arg(&socket)
        .env("CONTEXT_EMBEDDING_MODE", "stub")
        .env("CONTEXT_DAEMON_TTL_MS", "2000")
        .env("CONTEXT_DAEMON_CLEANUP_MS", "25")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn daemon-loop")?;

    wait_for_socket(&socket).await?;
    let proj = make_stub_project()?;

    ping(&socket, proj.path(), 150).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    ping(&socket, proj.path(), 150).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let st = status(&socket).await?;
    anyhow::ensure!(
        st.projects
            .iter()
            .any(|p| p.project == proj.path().to_string_lossy()),
        "expected project to still be active, got: {:?}",
        st.projects
    );

    tokio::time::sleep(Duration::from_millis(250)).await;
    let st2 = status(&socket).await?;
    anyhow::ensure!(st2.projects.is_empty());

    let _ = daemon.kill().await;
    let _ = daemon.wait().await;
    Ok(())
}

#[tokio::test]
async fn daemon_single_instance_does_not_break_live_socket() -> Result<()> {
    let sock_dir = tempdir()?;
    let socket = sock_dir.path().join("daemon.sock");

    let bin = assert_cmd::cargo::cargo_bin("context");
    let mut daemon1 = tokio::process::Command::new(&bin)
        .arg("daemon-loop")
        .arg("--socket")
        .arg(&socket)
        .env("CONTEXT_EMBEDDING_MODE", "stub")
        .env("CONTEXT_DAEMON_TTL_MS", "2000")
        .env("CONTEXT_DAEMON_CLEANUP_MS", "25")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn daemon-loop")?;

    wait_for_socket(&socket).await?;

    let mut daemon2 = tokio::process::Command::new(&bin)
        .arg("daemon-loop")
        .arg("--socket")
        .arg(&socket)
        .env("CONTEXT_EMBEDDING_MODE", "stub")
        .env("CONTEXT_DAEMON_TTL_MS", "2000")
        .env("CONTEXT_DAEMON_CLEANUP_MS", "25")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn daemon-loop (second instance)")?;

    // second instance should exit quickly (daemon already running)
    let mut waited = 0;
    while waited < 40 {
        if let Some(_status) = daemon2.try_wait()? {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        waited += 1;
    }
    anyhow::ensure!(
        daemon2.try_wait()?.is_some(),
        "second instance did not exit"
    );

    // socket should still be served by the first instance
    let st = status(&socket).await?;
    anyhow::ensure!(st.status == "ok");

    let _ = daemon1.kill().await;
    let _ = daemon1.wait().await;
    Ok(())
}

#[tokio::test]
async fn daemon_exits_when_idle_and_removes_socket() -> Result<()> {
    let sock_dir = tempdir()?;
    let socket = sock_dir.path().join("daemon.sock");

    let bin = assert_cmd::cargo::cargo_bin("context");
    let mut daemon = tokio::process::Command::new(bin)
        .arg("daemon-loop")
        .arg("--socket")
        .arg(&socket)
        .env("CONTEXT_EMBEDDING_MODE", "stub")
        .env("CONTEXT_DAEMON_TTL_MS", "150")
        .env("CONTEXT_DAEMON_CLEANUP_MS", "25")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn daemon-loop")?;

    wait_for_socket(&socket).await?;
    let proj = make_stub_project()?;

    // keep the project alive briefly, then let it expire + daemon idle out
    ping(&socket, proj.path(), 50).await?;

    tokio::time::sleep(Duration::from_millis(500)).await;
    anyhow::ensure!(
        UnixStream::connect(&socket).await.is_err(),
        "expected socket to be removed after idle exit"
    );

    // daemon should have exited on its own
    let mut waited = 0;
    while waited < 40 {
        if daemon.try_wait()?.is_some() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        waited += 1;
    }
    anyhow::bail!("daemon did not exit after idle TTL")
}
