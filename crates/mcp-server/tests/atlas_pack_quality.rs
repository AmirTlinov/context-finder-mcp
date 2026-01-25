use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RunningService, Service, ServiceExt},
    transport::TokioChildProcess,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
) -> Result<RunningService<rmcp::RoleClient, impl Service<rmcp::RoleClient>>> {
    let bin = locate_context_finder_mcp_bin()?;

    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_MODEL_DIR");
    cmd.env("CONTEXT_PROFILE", "quality");
    cmd.env("CONTEXT_EMBEDDING_MODE", "stub");
    cmd.env("RUST_LOG", "warn");
    cmd.env("CONTEXT_MCP_SHARED", "0");
    cmd.env("CONTEXT_DISABLE_DAEMON", "1");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")
}

async fn call_tool_text(
    service: &RunningService<rmcp::RoleClient, impl Service<rmcp::RoleClient>>,
    name: &str,
    args: serde_json::Value,
) -> Result<String> {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        service.call_tool(CallToolRequestParam {
            name: name.to_string().into(),
            arguments: args.as_object().cloned(),
        }),
    )
    .await
    .context("timeout calling tool")?
    .context("call tool")?;

    assert_ne!(result.is_error, Some(true), "{name} returned error");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("tool did not return text content")?;
    Ok(text.to_string())
}

async fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .await
        .with_context(|| format!("run git {:?} (cwd={})", args, root.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
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
        "failed to extract CP pack from atlas_pack output"
    );
    Ok(pack)
}

fn parse_cp_dict(pack: &str) -> Result<HashMap<String, String>> {
    let mut dict: HashMap<String, String> = HashMap::new();
    let mut in_dict = false;
    for line in pack.lines() {
        if line == "S DICT" {
            in_dict = true;
            continue;
        }
        if !in_dict {
            continue;
        }
        if line.starts_with("S ") {
            break;
        }
        let Some(rest) = line.strip_prefix("D ") else {
            continue;
        };
        let (id, raw) = rest.split_once(' ').context("malformed D line")?;
        let value: String = serde_json::from_str(raw).context("decode D value")?;
        dict.insert(id.to_string(), value);
    }
    Ok(dict)
}

fn map_paths_from_pack(pack: &str, dict: &HashMap<String, String>) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    let mut in_map = false;
    for line in pack.lines() {
        if line == "S MAP" {
            in_map = true;
            continue;
        }
        if !in_map {
            continue;
        }
        if line.starts_with("S ") {
            break;
        }
        if !line.starts_with("MAP ") {
            continue;
        }
        let path_id = line
            .split_whitespace()
            .find_map(|tok| tok.strip_prefix("path="))
            .unwrap_or("")
            .trim();
        if path_id.is_empty() {
            continue;
        }
        let value = dict
            .get(path_id)
            .cloned()
            .unwrap_or_else(|| path_id.to_string());
        paths.push(value);
    }
    Ok(paths)
}

#[tokio::test]
async fn atlas_pack_suppresses_obvious_noise_dirs_in_map() -> Result<()> {
    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    git(root, &["init"]).await?;
    git(root, &["config", "user.email", "test@example.com"]).await?;
    git(root, &["config", "user.name", "Test"]).await?;

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::create_dir_all(root.join("contracts").join("command").join("v1"))
        .context("mkdir contracts/command/v1")?;
    std::fs::create_dir_all(root.join(".github").join("workflows"))
        .context("mkdir .github/workflows")?;

    for scope in ["dist", "build", "out", "datasets", "logs"] {
        let dir = root.join(scope);
        std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {scope}"))?;
        for idx in 0..80u32 {
            std::fs::write(dir.join(format!("file_{idx:02}.txt")), "noise\n".repeat(3))
                .with_context(|| format!("write {scope}/file_{idx:02}.txt"))?;
        }
    }

    std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").context("write main")?;
    std::fs::write(
        root.join(".github").join("workflows").join("ci.yml"),
        "name: CI\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: cargo test --workspace\n",
    )
    .context("write ci.yml")?;
    std::fs::write(
        root.join("contracts")
            .join("command")
            .join("v1")
            .join("envelope.json"),
        r#"{"type":"object","properties":{"kind":{"type":"string"}}}"#,
    )
    .context("write envelope.json")?;
    std::fs::write(root.join("README.md"), "repo\n").context("write README")?;

    git(root, &["add", "."]).await?;
    git(root, &["commit", "-m", "init"]).await?;

    let service = start_mcp_server().await?;
    let text = call_tool_text(
        &service,
        "atlas_pack",
        serde_json::json!({
            "path": root.to_string_lossy(),
            "response_mode": "facts",
            "max_chars": 8000
        }),
    )
    .await?;

    let pack = extract_cp_pack(&text)?;
    let dict = parse_cp_dict(&pack)?;
    let map_paths = map_paths_from_pack(&pack, &dict)?;

    let forbid: HashSet<&str> = ["dist", "build", "out", "datasets", "logs"]
        .into_iter()
        .collect();
    for path in map_paths {
        let normalized = path.trim_end_matches('/').trim();
        assert!(
            !forbid.contains(normalized),
            "expected noise dir '{normalized}' to be suppressed from S MAP"
        );
    }

    Ok(())
}

#[tokio::test]
async fn atlas_pack_meaning_is_deterministic_for_same_root() -> Result<()> {
    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();

    git(root, &["init"]).await?;
    git(root, &["config", "user.email", "test@example.com"]).await?;
    git(root, &["config", "user.name", "Test"]).await?;

    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::create_dir_all(root.join(".github").join("workflows"))
        .context("mkdir .github/workflows")?;
    std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").context("write main")?;
    std::fs::write(
        root.join(".github").join("workflows").join("ci.yml"),
        "name: CI\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: cargo test --workspace\n",
    )
    .context("write ci.yml")?;
    git(root, &["add", "."]).await?;
    git(root, &["commit", "-m", "init"]).await?;

    let service = start_mcp_server().await?;
    let args = serde_json::json!({
        "path": root.to_string_lossy(),
        "query": "determinism check",
        "response_mode": "facts",
        "max_chars": 8000
    });

    let first_text = call_tool_text(&service, "atlas_pack", args.clone()).await?;
    let first_pack = extract_cp_pack(&first_text)?;
    assert!(
        first_pack.contains("ANCHOR "),
        "expected at least one ANCHOR in CPV1"
    );
    assert!(
        first_pack.contains("EV "),
        "expected at least one EV in CPV1"
    );

    let second_text = call_tool_text(&service, "atlas_pack", args).await?;
    let second_pack = extract_cp_pack(&second_text)?;

    assert_eq!(
        first_pack, second_pack,
        "expected deterministic meaning pack"
    );
    Ok(())
}
