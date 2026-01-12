use anyhow::{Context, Result};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use rmcp::{model::CallToolRequestParam, service::ServiceExt, transport::TokioChildProcess};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Barrier;

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

async fn spawn_server(
    bin: &Path,
    cursor_store_path: &Path,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>> {
    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    cmd.env(
        "CONTEXT_FINDER_MCP_CURSOR_STORE_PATH",
        cursor_store_path.to_string_lossy().to_string(),
    );

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")
}

#[tokio::test]
async fn cursor_alias_survives_process_restart() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let cursor_store_dir = tempfile::tempdir().context("temp cursor store dir")?;
    let cursor_store_path = cursor_store_dir.path().join("cursor_store.json");

    let tmp = tempfile::tempdir().context("temp project dir")?;
    let root = tmp.path();
    std::fs::write(root.join("README.md"), "one\ntwo\nthree\n").context("write README.md")?;

    // First server: request a paginated slice to obtain a compact cursor alias.
    let service_a = spawn_server(&bin, &cursor_store_path).await?;
    let args_a = serde_json::json!({
        "path": root.to_string_lossy(),
        "file": "README.md",
        "max_lines": 1,
        "max_chars": 2048,
        "response_mode": "minimal",
    });
    let resp_a = tokio::time::timeout(
        Duration::from_secs(10),
        service_a.call_tool(CallToolRequestParam {
            name: "file_slice".into(),
            arguments: args_a.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling file_slice on server A")??;

    let text_a = resp_a
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("missing file_slice text output from server A")?;
    let cursor = text_a
        .lines()
        .find_map(|line| line.strip_prefix("M: ").map(str::to_string))
        .context("missing next_cursor (M:) line in file_slice response A")?;
    assert!(
        cursor.starts_with("cfcs2:"),
        "expected compact cursor alias, got: {cursor:?}"
    );

    service_a.cancel().await.context("shutdown server A")?;

    // Second server: use the same cursor alias after restart, proving persistence.
    let service_b = spawn_server(&bin, &cursor_store_path).await?;
    let args_b = serde_json::json!({
        "path": root.to_string_lossy(),
        "cursor": cursor,
        "max_chars": 2048,
        "response_mode": "minimal",
    });
    let resp_b = tokio::time::timeout(
        Duration::from_secs(10),
        service_b.call_tool(CallToolRequestParam {
            name: "file_slice".into(),
            arguments: args_b.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling file_slice on server B")??;

    let text_b = resp_b
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("missing file_slice text output from server B")?;
    assert!(
        text_b.contains("two"),
        "expected second page to include \"two\""
    );

    service_b.cancel().await.context("shutdown server B")?;
    Ok(())
}

#[tokio::test]
async fn cursor_aliases_do_not_collide_across_concurrent_servers() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let cursor_store_dir = tempfile::tempdir().context("temp cursor store dir")?;
    let cursor_store_path = cursor_store_dir.path().join("cursor_store.json");

    let tmp_a = tempfile::tempdir().context("temp project A dir")?;
    let root_a = tmp_a.path();
    std::fs::write(root_a.join("README.md"), "a1\na2\na3\n").context("write A README.md")?;

    let tmp_b = tempfile::tempdir().context("temp project B dir")?;
    let root_b = tmp_b.path();
    std::fs::write(root_b.join("README.md"), "b1\nb2\nb3\n").context("write B README.md")?;

    let (service_a, service_b) = tokio::try_join!(
        spawn_server(&bin, &cursor_store_path),
        spawn_server(&bin, &cursor_store_path)
    )?;

    let barrier = Arc::new(Barrier::new(3));
    let call = |svc: rmcp::service::RunningService<rmcp::RoleClient, ()>,
                root: PathBuf,
                barrier: Arc<Barrier>| async move {
        barrier.wait().await;
        let args = serde_json::json!({
            "path": root.to_string_lossy(),
            "file": "README.md",
            "max_lines": 1,
            "max_chars": 2048,
            "response_mode": "minimal",
        });
        let resp = tokio::time::timeout(
            Duration::from_secs(10),
            svc.call_tool(CallToolRequestParam {
                name: "file_slice".into(),
                arguments: args.as_object().cloned(),
            }),
        )
        .await
        .context("timeout calling file_slice")?
        .context("call file_slice")?;

        let text = resp
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .context("missing file_slice text output")?;
        let cursor = text
            .lines()
            .find_map(|line| line.strip_prefix("M: ").map(str::to_string))
            .context("missing next_cursor (M:) line in file_slice output")?;
        svc.cancel().await.context("shutdown mcp service")?;
        Ok::<String, anyhow::Error>(cursor)
    };

    let task_a = tokio::spawn(call(service_a, root_a.to_path_buf(), barrier.clone()));
    let task_b = tokio::spawn(call(service_b, root_b.to_path_buf(), barrier.clone()));
    barrier.wait().await;

    let cursor_a = task_a.await.context("join cursor task A")??;
    let cursor_b = task_b.await.context("join cursor task B")??;
    assert!(
        cursor_a.starts_with("cfcs2:"),
        "expected compact cursor alias for A, got: {cursor_a:?}"
    );
    assert!(
        cursor_b.starts_with("cfcs2:"),
        "expected compact cursor alias for B, got: {cursor_b:?}"
    );
    assert_ne!(
        cursor_a, cursor_b,
        "expected distinct cursor aliases across concurrent servers"
    );

    // After both servers exit, a fresh process should be able to expand both cursor aliases from
    // the shared persisted cursor store file.
    let service_c = spawn_server(&bin, &cursor_store_path).await?;
    for (label, root, cursor, expected) in
        [("A", root_a, cursor_a, "a2"), ("B", root_b, cursor_b, "b2")]
    {
        let args = serde_json::json!({
            "path": root.to_string_lossy(),
            "cursor": cursor,
            "max_chars": 2048,
            "response_mode": "minimal",
        });
        let resp = tokio::time::timeout(
            Duration::from_secs(10),
            service_c.call_tool(CallToolRequestParam {
                name: "file_slice".into(),
                arguments: args.as_object().cloned(),
            }),
        )
        .await
        .with_context(|| format!("timeout calling file_slice on server C for {label}"))?
        .with_context(|| format!("call file_slice on server C for {label}"))?;

        let text = resp
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .context("missing file_slice text output from server C")?;
        assert!(
            text.contains(expected),
            "expected {label} continuation to include {expected:?}, got: {text}"
        );
    }
    service_c.cancel().await.context("shutdown server C")?;
    Ok(())
}

#[tokio::test]
async fn cursor_alias_signature_mismatch_fails_closed() -> Result<()> {
    let bin = locate_context_finder_mcp_bin()?;

    let cursor_store_dir = tempfile::tempdir().context("temp cursor store dir")?;
    let cursor_store_path = cursor_store_dir.path().join("cursor_store.json");

    let tmp = tempfile::tempdir().context("temp project dir")?;
    let root = tmp.path();
    std::fs::write(root.join("README.md"), "one\ntwo\nthree\n").context("write README.md")?;

    // Produce a compact cursor alias from a real server so the token has a valid signature.
    let service_a = spawn_server(&bin, &cursor_store_path).await?;
    let resp_a = tokio::time::timeout(
        Duration::from_secs(10),
        service_a.call_tool(CallToolRequestParam {
            name: "file_slice".into(),
            arguments: serde_json::json!({
                "path": root.to_string_lossy(),
                "file": "README.md",
                "max_lines": 1,
                "max_chars": 2048,
                "response_mode": "minimal",
            })
            .as_object()
            .cloned(),
        }),
    )
    .await
    .context("timeout calling file_slice on server A")??;
    let text_a = resp_a
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("missing file_slice text output from server A")?;
    let cursor = text_a
        .lines()
        .find_map(|line| line.strip_prefix("M: ").map(str::trim))
        .map(str::to_string)
        .context("missing next_cursor (M:) line in file_slice response A")?;
    assert!(
        cursor.starts_with("cfcs2:"),
        "expected cfcs2 cursor alias, got: {cursor:?}"
    );

    // Parse the store_id from the cursor alias (cfcs2 encodes u64 id + 6-byte signature).
    let encoded = cursor
        .strip_prefix("cfcs2:")
        .context("cursor missing cfcs2 prefix")?;
    let raw = URL_SAFE_NO_PAD
        .decode(encoded.as_bytes())
        .context("decode cfcs2 payload")?;
    let raw: [u8; 14] = raw
        .try_into()
        .map_err(|_| anyhow::anyhow!("unexpected cfcs2 payload length"))?;
    let store_id = u64::from_be_bytes(raw[..8].try_into().expect("8 bytes"));

    service_a.cancel().await.context("shutdown server A")?;

    // Corrupt the persisted cursor store entry for `store_id` so the signature check must fail.
    let bytes = std::fs::read(&cursor_store_path).context("read cursor store json")?;
    let mut store: serde_json::Value =
        serde_json::from_slice(&bytes).context("parse cursor store json")?;
    let entries = store
        .get_mut("entries")
        .and_then(|value| value.as_array_mut())
        .context("cursor store entries missing")?;
    let mut patched = false;
    for entry in entries {
        let id = entry.get("id").and_then(|value| value.as_u64());
        if id != Some(store_id) {
            continue;
        }
        let payload_b64 = STANDARD.encode(b"corrupted-cursor-payload");
        entry["payload_b64"] = serde_json::Value::String(payload_b64);
        patched = true;
        break;
    }
    assert!(patched, "failed to locate cursor store entry {store_id}");
    std::fs::write(
        &cursor_store_path,
        serde_json::to_vec(&store).context("encode store json")?,
    )
    .context("write corrupted cursor store json")?;

    // New process: expanding the original cursor alias must now fail closed (expired), never
    // returning a different cursor string.
    let service_b = spawn_server(&bin, &cursor_store_path).await?;
    let resp_b = tokio::time::timeout(
        Duration::from_secs(10),
        service_b.call_tool(CallToolRequestParam {
            name: "file_slice".into(),
            arguments: serde_json::json!({
                "path": root.to_string_lossy(),
                "cursor": cursor,
                "max_chars": 2048,
                "response_mode": "minimal",
            })
            .as_object()
            .cloned(),
        }),
    )
    .await
    .context("timeout calling file_slice on server B")??;

    assert_eq!(
        resp_b.is_error,
        Some(true),
        "expected corrupted cursor alias expansion to fail"
    );
    let text_b = resp_b
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or_default();
    assert!(
        text_b.contains("expired continuation"),
        "expected 'expired continuation' error, got: {text_b}"
    );

    service_b.cancel().await.context("shutdown server B")?;
    Ok(())
}
