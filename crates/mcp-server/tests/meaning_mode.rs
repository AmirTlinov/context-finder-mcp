use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use std::collections::HashSet;
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

async fn start_mcp_server(
) -> Result<RunningService<RoleClient, impl rmcp::service::Service<RoleClient>>> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")?;
    Ok(service)
}

async fn call_tool(
    service: &RunningService<RoleClient, impl rmcp::service::Service<RoleClient>>,
    name: &str,
    args: serde_json::Value,
) -> Result<rmcp::model::CallToolResult> {
    tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: name.to_string().into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling tool")?
    .context("call tool")
}

fn extract_cp_pack(text: &str) -> Result<String> {
    let mut pack = String::new();
    let mut in_pack = false;
    for line in text.lines() {
        if !in_pack {
            if line == "CPV1" {
                in_pack = true;
            } else {
                continue;
            }
        }
        if line.starts_with("N: ") {
            break;
        }
        pack.push_str(line);
        pack.push('\n');
    }
    anyhow::ensure!(
        !pack.is_empty(),
        "failed to extract CP pack from text output"
    );
    Ok(pack)
}

fn extract_ev_ref(line: &str) -> Option<&str> {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix("ev="))
}

fn assert_meaning_invariants(pack: &str) -> Result<()> {
    let mut ev_ids: HashSet<&str> = HashSet::new();
    for line in pack.lines() {
        if line.starts_with("EV ") {
            let Some(id) = line
                .strip_prefix("EV ")
                .and_then(|rest| rest.split_whitespace().next())
            else {
                continue;
            };
            ev_ids.insert(id);
            anyhow::ensure!(
                line.contains(" sha256="),
                "expected EV line to include sha256= (got: {line})"
            );
        }
    }
    anyhow::ensure!(!ev_ids.is_empty(), "expected at least one EV line");

    for line in pack.lines() {
        let is_claim = line.starts_with("ENTRY ")
            || line.starts_with("CONTRACT ")
            || line.starts_with("BOUNDARY ")
            || line.starts_with("FLOW ")
            || line.starts_with("BROKER ");
        if !is_claim {
            continue;
        }
        let Some(ev) = extract_ev_ref(line) else {
            anyhow::bail!("claim missing ev= pointer: {line}");
        };
        anyhow::ensure!(
            ev_ids.contains(ev),
            "claim references missing EV ({ev}): {line}"
        );
    }

    let nba = pack
        .lines()
        .find(|line| line.starts_with("NBA "))
        .context("expected NBA line in CP")?;
    anyhow::ensure!(
        nba.contains("evidence_fetch"),
        "expected NBA to suggest evidence_fetch (got: {nba})"
    );
    let Some(ev) = extract_ev_ref(nba) else {
        anyhow::bail!("NBA missing ev= pointer: {nba}");
    };
    anyhow::ensure!(
        ev_ids.contains(ev),
        "NBA references missing EV ({ev}): {nba}"
    );

    Ok(())
}

#[tokio::test]
async fn meaning_pack_is_bounded_and_has_cp_header() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    std::fs::create_dir_all(root.path().join("src")).context("mkdir src")?;
    std::fs::create_dir_all(root.path().join("contracts"))
        .context("mkdir contracts (for contract detection)")?;
    std::fs::write(root.path().join("src").join("main.rs"), "fn main() {}\n")
        .context("write src/main.rs")?;
    std::fs::write(
        root.path().join("contracts").join("example.schema.json"),
        "{ \"type\": \"object\" }\n",
    )
    .context("write contracts/example.schema.json")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "query": "orient on entrypoints and contracts",
            "max_chars": 1200,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        resp.is_error,
        Some(true),
        "expected meaning_pack to succeed"
    );

    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_pack missing text output")?;
    assert!(
        text.contains("\nCPV1\n") || text.contains("\nCPV1\r\n"),
        "expected CPV1"
    );
    assert!(
        text.contains("\nROOT_FP ") || text.contains("\r\nROOT_FP "),
        "expected ROOT_FP"
    );
    assert!(text.contains("\nS ENTRYPOINTS\n") || text.contains("\r\nS ENTRYPOINTS\r\n"));
    assert!(text.contains("\nS CONTRACTS\n") || text.contains("\r\nS CONTRACTS\r\n"));

    let pack = extract_cp_pack(text)?;
    let max_chars = 1200usize;
    let used_chars = pack.chars().count();
    assert!(
        used_chars <= max_chars,
        "expected used_chars <= max_chars (used={used_chars}, max={max_chars})"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_focus_emits_focus_section_and_is_bounded() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    std::fs::create_dir_all(root.path().join("src")).context("mkdir src")?;
    std::fs::write(root.path().join("src").join("main.rs"), "fn main() {}\n")
        .context("write src/main.rs")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_focus",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "focus": "src/main.rs",
            "max_chars": 1200,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        resp.is_error,
        Some(true),
        "expected meaning_focus to succeed"
    );

    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_focus missing text output")?;
    assert!(
        text.contains("\nCPV1\n") || text.contains("\nCPV1\r\n"),
        "expected CPV1"
    );
    assert!(
        text.contains("\nS FOCUS\n") || text.contains("\r\nS FOCUS\r\n"),
        "expected S FOCUS section"
    );

    let pack = extract_cp_pack(text)?;
    assert!(
        pack.contains("S OUTLINE"),
        "expected S OUTLINE section in CP"
    );
    assert!(
        pack.lines().any(|line| line.starts_with("SYM ")),
        "expected at least one SYM line in CP"
    );
    let max_chars = 1200usize;
    let used_chars = pack.chars().count();
    assert!(
        used_chars <= max_chars,
        "expected used_chars <= max_chars (used={used_chars}, max={max_chars})"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_pack_claims_have_evidence_and_refs_resolve() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    std::fs::create_dir_all(root.path().join("src")).context("mkdir src")?;
    std::fs::create_dir_all(root.path().join("contracts"))
        .context("mkdir contracts (for contract detection)")?;
    std::fs::write(root.path().join("src").join("main.rs"), "fn main() {}\n")
        .context("write src/main.rs")?;
    std::fs::write(
        root.path().join("contracts").join("example.schema.json"),
        "{ \"type\": \"object\" }\n",
    )
    .context("write contracts/example.schema.json")?;
    std::fs::write(root.path().join(".env.example"), "EXAMPLE=1\n")
        .context("write .env.example")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "query": "verify evidence coverage",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        resp.is_error,
        Some(true),
        "expected meaning_pack to succeed"
    );

    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_pack missing text output")?;
    let pack = extract_cp_pack(text)?;
    assert_meaning_invariants(&pack)?;

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_pack_detects_asyncapi_contract_and_event_boundary() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    std::fs::create_dir_all(root.path().join("src")).context("mkdir src")?;
    std::fs::write(root.path().join("src").join("main.rs"), "fn main() {}\n")
        .context("write src/main.rs")?;
    std::fs::write(
        root.path().join("asyncapi.yaml"),
        "asyncapi: 2.6.0\ninfo:\n  title: Example\n  version: 1.0.0\nservers:\n  local:\n    url: localhost:9092\n    protocol: kafka\nchannels:\n  user.created:\n    publish:\n      message:\n        name: UserCreated\n  user.deleted:\n    subscribe:\n      message:\n        name: UserDeleted\n",
    )
    .context("write asyncapi.yaml")?;
    std::fs::create_dir_all(root.path().join("k8s")).context("mkdir k8s")?;
    std::fs::write(
        root.path().join("k8s").join("kafka.yaml"),
        "apiVersion: v1\nkind: Pod\nmetadata:\n  name: kafka\nspec:\n  containers:\n  - name: kafka\n    image: bitnami/kafka:latest\n",
    )
    .context("write k8s/kafka.yaml")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "query": "detect event-driven contract and boundary",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        resp.is_error,
        Some(true),
        "expected meaning_pack to succeed"
    );

    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_pack missing text output")?;
    let pack = extract_cp_pack(text)?;
    assert_meaning_invariants(&pack)?;

    assert!(
        pack.contains("CONTRACT kind=asyncapi"),
        "expected asyncapi contract kind in CP"
    );
    assert!(
        pack.contains("BOUNDARY kind=event"),
        "expected event boundary kind in CP"
    );
    assert!(pack.contains("S FLOWS"), "expected S FLOWS section in CP");
    assert!(
        pack.lines().any(|line| line.starts_with("FLOW ")),
        "expected at least one FLOW line in CP"
    );
    assert!(
        pack.contains("proto=kafka"),
        "expected FLOW line to include proto=kafka"
    );
    assert!(
        pack.contains("S BROKERS"),
        "expected S BROKERS section in CP"
    );
    assert!(
        pack.lines().any(|line| line.starts_with("BROKER ")),
        "expected at least one BROKER line in CP"
    );
    assert!(
        pack.contains("BROKER proto=kafka"),
        "expected BROKER line to include proto=kafka"
    );
    assert!(pack.contains("dir=pub"), "expected publish flow (dir=pub)");
    assert!(
        pack.contains("dir=sub"),
        "expected subscribe flow (dir=sub)"
    );
    assert!(
        pack.contains("user.created"),
        "expected channel name in dict"
    );
    assert!(
        pack.contains("user.deleted"),
        "expected channel name in dict"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_focus_claims_have_evidence_and_refs_resolve() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    std::fs::create_dir_all(root.path().join("src")).context("mkdir src")?;
    std::fs::create_dir_all(root.path().join("contracts"))
        .context("mkdir contracts (for contract detection)")?;
    std::fs::write(root.path().join("src").join("main.rs"), "fn main() {}\n")
        .context("write src/main.rs")?;
    std::fs::write(
        root.path().join("contracts").join("example.schema.json"),
        "{ \"type\": \"object\" }\n",
    )
    .context("write contracts/example.schema.json")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_focus",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "focus": "src",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        resp.is_error,
        Some(true),
        "expected meaning_focus to succeed"
    );

    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_focus missing text output")?;
    let pack = extract_cp_pack(text)?;
    assert_meaning_invariants(&pack)?;

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_focus_rejects_escape_outside_root() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    std::fs::create_dir_all(root.path().join("src")).context("mkdir src")?;
    std::fs::write(root.path().join("src").join("main.rs"), "fn main() {}\n")
        .context("write src/main.rs")?;

    let parent = root.path().parent().context("tempdir must have a parent")?;
    let outside = tempfile::tempdir_in(parent).context("temp outside dir")?;
    std::fs::write(outside.path().join("evil.txt"), "nope\n").context("write evil.txt")?;
    let outside_name = outside
        .path()
        .file_name()
        .context("outside dir must have a name")?
        .to_string_lossy();
    let focus = format!("../{outside_name}/evil.txt");

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_focus",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "focus": focus,
            "max_chars": 1200,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_eq!(
        resp.is_error,
        Some(true),
        "expected meaning_focus to reject outside-root focus"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_focus_rejects_secret_paths() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    std::fs::write(root.path().join(".env"), "SECRET=1\n").context("write .env")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_focus",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "focus": ".env",
            "max_chars": 1200,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_eq!(
        resp.is_error,
        Some(true),
        "expected meaning_focus to reject secret focus"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn evidence_fetch_sets_stale_on_hash_mismatch() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    std::fs::create_dir_all(root.path().join("src")).context("mkdir src")?;
    std::fs::write(
        root.path().join("src").join("main.rs"),
        "fn main() {\n  println!(\"hi\");\n}\n",
    )
    .context("write src/main.rs")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "evidence_fetch",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "items": [{
                "file": "src/main.rs",
                "start_line": 1,
                "end_line": 2,
                "source_hash": "0000"
            }],
            "max_chars": 2000,
            "max_lines": 50,
            "strict_hash": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        resp.is_error,
        Some(true),
        "expected evidence_fetch to succeed"
    );
    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("evidence_fetch missing text output")?;
    assert!(
        text.contains("R: src/main.rs:1 evidence"),
        "expected evidence ref header"
    );
    assert!(
        text.contains("N: source_hash="),
        "expected source_hash note"
    );
    assert!(text.contains("N: stale=true"), "expected stale=true note");

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn evidence_fetch_strict_hash_errors_on_mismatch() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    std::fs::create_dir_all(root.path().join("src")).context("mkdir src")?;
    std::fs::write(root.path().join("src").join("main.rs"), "fn main() {}\n")
        .context("write src/main.rs")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "evidence_fetch",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "items": [{
                "file": "src/main.rs",
                "start_line": 1,
                "end_line": 1,
                "source_hash": "0000"
            }],
            "max_chars": 2000,
            "max_lines": 50,
            "strict_hash": true,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_eq!(
        resp.is_error,
        Some(true),
        "expected evidence_fetch strict_hash mismatch to error"
    );
    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or_default();
    assert!(
        text.contains("source_hash") || text.contains("mismatch"),
        "expected mismatch message, got: {text}"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}
