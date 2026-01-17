use anyhow::{Context, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::process::Command;

#[derive(Debug)]
struct Args {
    root: PathBuf,
    max_depth: usize,
    limit: usize,
    max_chars: usize,
    response_mode: String,
    tools: Vec<String>,
    include_worktrees: bool,
    out_json: Option<PathBuf>,
    out_md: Option<PathBuf>,
    strict: bool,
    strict_max_latency_ms: u64,
    strict_max_noise_ratio: f64,
    strict_min_token_saved: f64,
    strict_min_baseline_chars: usize,
}

fn parse_args() -> Result<Args> {
    let mut root: Option<PathBuf> = None;
    let mut max_depth: usize = 4;
    let mut limit: usize = 30;
    let mut max_chars: usize = 2000;
    let mut response_mode = "facts".to_string();
    let mut tools: Vec<String> = Vec::new();
    let mut include_worktrees = false;
    let mut out_json: Option<PathBuf> = None;
    let mut out_md: Option<PathBuf> = None;
    let mut strict = false;
    let mut strict_max_latency_ms: u64 = 5_000;
    let mut strict_max_noise_ratio: f64 = 0.50;
    let mut strict_min_token_saved: f64 = 0.20;
    let mut strict_min_baseline_chars: usize = 2_000;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--root" => {
                let val = it.next().context("--root requires a path")?;
                root = Some(PathBuf::from(val));
            }
            "--max-depth" => {
                let val = it.next().context("--max-depth requires a number")?;
                max_depth = val.parse::<usize>().context("parse --max-depth")?;
            }
            "--limit" => {
                let val = it.next().context("--limit requires a number")?;
                limit = val.parse::<usize>().context("parse --limit")?;
            }
            "--max-chars" => {
                let val = it.next().context("--max-chars requires a number")?;
                max_chars = val.parse::<usize>().context("parse --max-chars")?;
            }
            "--response-mode" => {
                response_mode = it.next().context("--response-mode requires a value")?;
            }
            "--tool" => {
                let val = it.next().context("--tool requires a tool name")?;
                tools.push(val);
            }
            "--include-worktrees" => include_worktrees = true,
            "--strict-max-latency-ms" => {
                let val = it
                    .next()
                    .context("--strict-max-latency-ms requires a number")?;
                strict_max_latency_ms = val
                    .parse::<u64>()
                    .context("parse --strict-max-latency-ms")?;
            }
            "--strict-max-noise-ratio" => {
                let val = it
                    .next()
                    .context("--strict-max-noise-ratio requires a float")?;
                strict_max_noise_ratio = val
                    .parse::<f64>()
                    .context("parse --strict-max-noise-ratio")?;
            }
            "--strict-min-token-saved" => {
                let val = it
                    .next()
                    .context("--strict-min-token-saved requires a float")?;
                strict_min_token_saved = val
                    .parse::<f64>()
                    .context("parse --strict-min-token-saved")?;
            }
            "--strict-min-baseline-chars" => {
                let val = it
                    .next()
                    .context("--strict-min-baseline-chars requires a number")?;
                strict_min_baseline_chars = val
                    .parse::<usize>()
                    .context("parse --strict-min-baseline-chars")?;
            }
            "--out-json" => {
                let val = it.next().context("--out-json requires a path")?;
                out_json = Some(PathBuf::from(val));
            }
            "--out-md" => {
                let val = it.next().context("--out-md requires a path")?;
                out_md = Some(PathBuf::from(val));
            }
            "--strict" => strict = true,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => anyhow::bail!("Unknown arg: {other} (use --help)"),
        }
    }

    let root = root.unwrap_or_else(|| PathBuf::from("/home/amir/Документы/projects"));
    if tools.is_empty() {
        tools.push("meaning_pack".to_string());
        tools.push("atlas_pack".to_string());
    }

    Ok(Args {
        root,
        max_depth,
        limit,
        max_chars,
        response_mode,
        tools,
        include_worktrees,
        out_json,
        out_md,
        strict,
        strict_max_latency_ms,
        strict_max_noise_ratio,
        strict_min_token_saved,
        strict_min_baseline_chars,
    })
}

fn print_help() {
    eprintln!(
        "\
context-mcp-eval-zoo

Runs a real-repo quality “zoo” over many git repos (meaning_pack + atlas_pack by default).

Usage:
  context-mcp-eval-zoo --root <dir> [--limit N] [--max-depth N] [--max-chars N]
                      [--tool meaning_pack] [--tool atlas_pack]
                      [--include-worktrees]
                      [--strict-max-latency-ms N]
                      [--strict-max-noise-ratio F]
                      [--strict-min-token-saved F]
                      [--strict-min-baseline-chars N]
                      [--out-json <path>] [--out-md <path>] [--strict]

Defaults:
  --root /home/amir/Документы/projects
  --limit 30
  --max-depth 4
  --max-chars 2000
  --tool meaning_pack --tool atlas_pack
  --response-mode facts
  --include-worktrees false
  --strict-max-latency-ms 5000
  --strict-max-noise-ratio 0.50
  --strict-min-token-saved 0.20
  --strict-min-baseline-chars 2000
"
    );
}

fn locate_context_finder_mcp_bin() -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            for name in ["context-finder-mcp", "context-mcp"] {
                let candidate = exe_dir.join(name);
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }

        if let Some(target_profile_dir) = exe.parent().and_then(|p| p.parent()) {
            for name in ["context-finder-mcp", "context-mcp"] {
                let candidate = target_profile_dir.join(name);
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }

    for name in ["context-finder-mcp", "context-mcp"] {
        if which_in_path(name).is_some() {
            return Ok(PathBuf::from(name));
        }
    }

    anyhow::bail!("failed to locate context-finder-mcp/context-mcp binary (build or install it)")
}

fn which_in_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

async fn start_mcp_server(
) -> Result<RunningService<RoleClient, impl rmcp::service::Service<RoleClient>>> {
    let bin = locate_context_finder_mcp_bin()?;
    let mut cmd = Command::new(bin);
    cmd.env_remove("CONTEXT_FINDER_MODEL_DIR");
    cmd.env("CONTEXT_FINDER_PROFILE", "quality");
    cmd.env("CONTEXT_FINDER_EMBEDDING_MODE", "stub");
    cmd.env("CONTEXT_FINDER_MCP_SHARED", "0");
    cmd.env("CONTEXT_FINDER_DISABLE_DAEMON", "1");
    cmd.env("RUST_LOG", "warn");

    let transport = TokioChildProcess::new(cmd).context("spawn mcp server")?;
    let service = tokio::time::timeout(Duration::from_secs(20), ().serve(transport))
        .await
        .context("timeout starting MCP server")?
        .context("start MCP server")?;
    Ok(service)
}

async fn call_tool(
    service: &RunningService<RoleClient, impl rmcp::service::Service<RoleClient>>,
    name: &str,
    args: serde_json::Value,
    timeout: Duration,
) -> Result<rmcp::model::CallToolResult> {
    tokio::time::timeout(
        timeout,
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
                pack.push_str("CPV1\n");
            }
            continue;
        }
        pack.push_str(line);
        pack.push('\n');
    }
    anyhow::ensure!(in_pack, "missing CPV1 in tool output");
    Ok(pack)
}

fn parse_cp_dict(pack: &str) -> Result<HashMap<String, String>> {
    let mut out: HashMap<String, String> = HashMap::new();
    for line in pack.lines() {
        if !line.starts_with("D ") {
            continue;
        }
        let mut parts = line.splitn(3, ' ');
        parts.next(); // "D"
        let Some(id) = parts.next() else { continue };
        let Some(rest) = parts.next() else { continue };
        let value: String =
            serde_json::from_str(rest).with_context(|| format!("parse dict json: {line}"))?;
        out.insert(id.to_string(), value);
    }
    Ok(out)
}

fn parse_ev_file_and_range(ev_line: &str) -> Option<(&str, usize, usize)> {
    if !ev_line.starts_with("EV ") {
        return None;
    }
    let file = ev_line
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("file="))?;
    let range = ev_line
        .split_whitespace()
        .find(|tok| tok.starts_with('L') && tok.contains("-L"))?;
    let rest = range.strip_prefix('L')?;
    let mut parts = rest.split("-L");
    let start = parts.next()?.parse::<usize>().ok()?;
    let end = parts.next()?.parse::<usize>().ok()?;
    Some((file, start, end))
}

fn count_file_slice_chars(
    root: &Path,
    rel: &str,
    start_line: usize,
    end_line: usize,
) -> Result<usize> {
    if start_line == 0 || end_line == 0 || end_line < start_line {
        return Ok(0);
    }

    // The real-repo zoo must be robust to binary / non-UTF8 files and huge files. We only read up to
    // end_line and count bytes as a stable approximation (pack is ASCII-heavy).
    let path = root.join(rel);
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Ok(0),
    };
    let mut reader = std::io::BufReader::new(file);
    let mut buf: Vec<u8> = Vec::new();

    let mut line_no = 0usize;
    let mut total = 0usize;
    loop {
        buf.clear();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(_) => return Ok(total),
        };
        if n == 0 {
            break;
        }
        line_no = line_no.saturating_add(1);
        if line_no < start_line {
            continue;
        }
        if line_no > end_line {
            break;
        }

        // Mimic `String::lines()` behavior + `+1` per line for a newline separator.
        let mut line_len = buf.len();
        if line_len > 0 && buf[line_len - 1] == b'\n' {
            line_len = line_len.saturating_sub(1);
        }
        if line_len > 0 && buf[line_len - 1] == b'\r' {
            line_len = line_len.saturating_sub(1);
        }
        total = total.saturating_add(line_len);
        total = total.saturating_add(1);
    }
    Ok(total)
}

fn is_noise_map_dir(dir: &str) -> bool {
    let lc = dir.trim().trim_start_matches("./").to_ascii_lowercase();
    let basename = lc.rsplit('/').next().unwrap_or(&lc);
    matches!(
        basename,
        "dist"
            | "build"
            | "out"
            | "target"
            | "node_modules"
            | "data"
            | "dataset"
            | "datasets"
            | "artifacts"
            | "results"
            | "corpus"
            | ".worktrees"
            | ".cache"
            | ".venv"
            | ".mypy_cache"
            | ".pytest_cache"
            | ".ruff_cache"
    ) || lc.starts_with("dist/")
        || lc.starts_with("build/")
        || lc.starts_with("out/")
        || lc.starts_with("target/")
        || lc.starts_with("node_modules/")
        || lc.starts_with("data/")
        || lc.starts_with("dataset/")
        || lc.starts_with("datasets/")
        || lc.starts_with("artifacts/")
        || lc.starts_with("results/")
        || lc.starts_with("corpus/")
        || lc.starts_with(".worktrees/")
        || lc.starts_with(".cache/")
}

#[derive(Debug, Default)]
struct MapStats {
    area_entries: usize,
    map_entries: usize,
    map_noise_entries: usize,
    output_area_entries: usize,
}

fn compute_map_stats(pack: &str, dict: &HashMap<String, String>) -> MapStats {
    let mut stats = MapStats::default();
    let mut section: Option<&str> = None;

    for line in pack.lines() {
        if line == "S MAP" {
            section = Some("map");
            continue;
        }
        if line == "S OUTPUTS" {
            section = Some("outputs");
            continue;
        }
        if line.starts_with("S ") {
            section = None;
            continue;
        }

        match section {
            Some("map") => {
                if line.starts_with("AREA ") {
                    stats.area_entries = stats.area_entries.saturating_add(1);
                    continue;
                }
                if !line.starts_with("MAP ") {
                    continue;
                }
                stats.map_entries = stats.map_entries.saturating_add(1);
                let path_id = line
                    .split_whitespace()
                    .find_map(|tok| tok.strip_prefix("path="))
                    .unwrap_or("");
                let path = dict.get(path_id).cloned().unwrap_or_default();
                if is_noise_map_dir(&path) {
                    stats.map_noise_entries = stats.map_noise_entries.saturating_add(1);
                }
            }
            Some("outputs") => {
                if line.starts_with("AREA ") {
                    stats.output_area_entries = stats.output_area_entries.saturating_add(1);
                }
            }
            _ => {}
        }
    }

    stats
}

#[derive(Debug, Default)]
struct TokenSavedCalc {
    token_saved: Option<f64>,
    baseline_chars: Option<usize>,
    used_chars: usize,
    ev_slices: usize,
    anchor_files: usize,
}

fn compute_token_saved(
    root: &Path,
    pack: &str,
    dict: &HashMap<String, String>,
) -> Result<TokenSavedCalc> {
    let used_chars = pack.chars().count();

    let mut seen: HashSet<String> = HashSet::new();
    let mut baseline_ev_chars = 0usize;
    let mut ev_slices = 0usize;
    for line in pack.lines() {
        let Some((file_id, start, end)) = parse_ev_file_and_range(line) else {
            continue;
        };
        let Some(rel) = dict.get(file_id) else {
            continue;
        };
        let key = format!("{rel}:{start}-{end}");
        if !seen.insert(key) {
            continue;
        }
        ev_slices = ev_slices.saturating_add(1);
        baseline_ev_chars =
            baseline_ev_chars.saturating_add(count_file_slice_chars(root, rel, start, end)?);
    }

    let mut anchor_files: HashSet<String> = HashSet::new();
    let mut in_anchors = false;
    for line in pack.lines() {
        if line == "S ANCHORS" {
            in_anchors = true;
            continue;
        }
        if !in_anchors {
            continue;
        }
        if line.starts_with("S ") {
            break;
        }
        if !line.starts_with("ANCHOR ") {
            continue;
        }
        let file_id = line
            .split_whitespace()
            .find_map(|tok| tok.strip_prefix("file="))
            .unwrap_or("");
        if let Some(rel) = dict.get(file_id) {
            anchor_files.insert(rel.clone());
        }
    }
    let anchor_files_count = anchor_files.len();

    let mut baseline_anchor_chars = 0usize;
    for rel in anchor_files {
        baseline_anchor_chars =
            baseline_anchor_chars.saturating_add(count_file_slice_chars(root, &rel, 1, 200)?);
    }

    let baseline = baseline_ev_chars.max(baseline_anchor_chars);
    if baseline == 0 {
        return Ok(TokenSavedCalc {
            token_saved: None,
            baseline_chars: None,
            used_chars,
            ev_slices,
            anchor_files: anchor_files_count,
        });
    }

    Ok(TokenSavedCalc {
        token_saved: Some(1.0 - (used_chars as f64 / baseline as f64)),
        baseline_chars: Some(baseline),
        used_chars,
        ev_slices,
        anchor_files: anchor_files_count,
    })
}

fn discover_git_roots(
    root: &Path,
    max_depth: usize,
    limit: usize,
    include_worktrees: bool,
) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut seen: HashSet<PathBuf> = HashSet::new();

    while let Some((dir, depth)) = stack.pop() {
        if out.len() >= limit {
            break;
        }
        if !seen.insert(dir.clone()) {
            continue;
        }
        let name = dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if matches!(
            name,
            ".git"
                | "node_modules"
                | "target"
                | "dist"
                | "build"
                | "out"
                | ".cache"
                | ".venv"
                | ".fastembed_cache"
                | ".deps"
                | ".context"
                | ".context-finder"
        ) {
            continue;
        }
        if !include_worktrees && name == ".worktrees" {
            continue;
        }

        if dir.join(".git").exists() {
            out.push(dir.clone());
            if include_worktrees && depth < max_depth {
                let worktrees = dir.join(".worktrees");
                if worktrees.is_dir() {
                    stack.push((worktrees, depth + 1));
                }
            }
            continue;
        }
        if depth >= max_depth {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push((path, depth + 1));
            }
        }
    }

    out.sort();
    Ok(out)
}

fn git_head_short(repo: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn detect_archetypes(repo: &Path) -> Vec<String> {
    let mut tags = Vec::new();
    let has = |rel: &str| repo.join(rel).exists();

    let mut langs = 0usize;
    if has("Cargo.toml") {
        langs += 1;
        tags.push("rust".to_string());
    }
    if has("package.json") {
        langs += 1;
        tags.push("node".to_string());
    }
    if has("pyproject.toml") {
        langs += 1;
        tags.push("python".to_string());
    }
    if has("go.mod") {
        langs += 1;
        tags.push("go".to_string());
    }
    if has("proto") || has("proto/") {
        tags.push("proto".to_string());
    }
    if has("notebooks") || has("experiments") || has("baselines") {
        tags.push("research".to_string());
    }
    if has("data") || has("datasets") {
        tags.push("dataset-heavy".to_string());
    }
    if langs >= 2 {
        tags.push("polyglot".to_string());
    }
    if has("crates") || has("packages") || has("apps") {
        tags.push("monorepo".to_string());
    }
    tags
}

fn choose_query(archetypes: &[String]) -> String {
    if archetypes.iter().any(|t| t == "research") {
        "orient on canon, experiments, artifacts, datasets, and how-to-run".to_string()
    } else {
        "orient on entrypoints, contracts, CI gates, and how-to-run tests".to_string()
    }
}

#[derive(Debug, Serialize)]
struct ToolMetrics {
    ok: bool,
    error: Option<String>,
    latency_ms: u128,
    stable: bool,
    strict_ok: bool,
    strict_violations: Vec<String>,
    area_entries: usize,
    map_entries: usize,
    map_noise_entries: usize,
    noise_ratio: Option<f64>,
    output_area_entries: usize,
    ev_slices: usize,
    anchor_files: usize,
    used_chars: usize,
    baseline_chars: Option<usize>,
    token_saved: Option<f64>,
}

#[derive(Debug, Serialize)]
struct RepoResult {
    path: String,
    head: Option<String>,
    archetypes: Vec<String>,
    tools: HashMap<String, ToolMetrics>,
}

#[derive(Debug, Serialize)]
struct StrictPolicy {
    max_latency_ms: u64,
    max_noise_ratio: f64,
    min_token_saved: f64,
    min_baseline_chars: usize,
}

#[derive(Debug, Serialize)]
struct ZooReport {
    root: String,
    limit: usize,
    max_depth: usize,
    max_chars: usize,
    response_mode: String,
    include_worktrees: bool,
    strict: bool,
    strict_policy: StrictPolicy,
    repos: Vec<RepoResult>,
}

fn percentile(sorted: &[u128], pct: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (sorted.len().saturating_sub(1)).saturating_mul(pct) / 100;
    sorted[idx]
}

fn render_md(report: &ZooReport) -> String {
    let mut out = String::new();
    out.push_str("# Meaning/Atlas Zoo Report (real repos)\n\n");
    out.push_str(&format!(
        "- root: `{}`\n- repos: `{}`\n- tools: `{}`\n- include_worktrees: `{}`\n- strict: `{}`\n- strict_policy: latency_ms<={} noise_ratio<={:.3} token_saved>={:.3} baseline>={}\n\n",
        report.root,
        report.repos.len(),
        report
            .repos
            .first()
            .map(|r| r.tools.keys().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_else(|| "-".to_string()),
        report.include_worktrees,
        report.strict,
        report.strict_policy.max_latency_ms,
        report.strict_policy.max_noise_ratio,
        report.strict_policy.min_token_saved,
        report.strict_policy.min_baseline_chars
    ));

    out.push_str(
        "| repo | head | archetypes | tool | ok | strict_ok | stable | latency_ms | areas_n | map_n | noise_ratio | outputs_n | ev_n | anchor_n | baseline | used | token_saved | violations |\n",
    );
    out.push_str(
        "|---|---:|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|\n",
    );
    for repo in &report.repos {
        for (tool, m) in &repo.tools {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                repo.path,
                repo.head.clone().unwrap_or_else(|| "-".to_string()),
                if repo.archetypes.is_empty() {
                    "-".to_string()
                } else {
                    repo.archetypes.join(",")
                },
                tool,
                if m.ok { "yes" } else { "no" },
                if m.strict_ok { "yes" } else { "no" },
                if m.stable { "yes" } else { "no" },
                m.latency_ms,
                m.area_entries,
                m.map_entries,
                m.noise_ratio
                    .map(|v| format!("{:.4}", v))
                    .unwrap_or_else(|| "-".to_string()),
                m.output_area_entries,
                m.ev_slices,
                m.anchor_files,
                m.baseline_chars
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                m.used_chars,
                m.token_saved
                    .map(|v| format!("{:.4}", v))
                    .unwrap_or_else(|| "-".to_string()),
                if m.strict_violations.is_empty() {
                    "-".to_string()
                } else {
                    m.strict_violations.join(";")
                }
            ));
        }
    }
    out
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    anyhow::ensure!(
        args.root.exists(),
        "root directory does not exist: {}",
        args.root.display()
    );

    let repos = discover_git_roots(
        &args.root,
        args.max_depth,
        args.limit,
        args.include_worktrees,
    )?;
    if repos.is_empty() {
        anyhow::bail!(
            "No git repos found under {} (try a different --root or higher --max-depth)",
            args.root.display()
        );
    }

    let service = start_mcp_server().await?;

    let mut results: Vec<RepoResult> = Vec::new();
    let mut strict_failed = false;

    let mut all_latencies: Vec<u128> = Vec::new();
    let mut all_noise: Vec<f64> = Vec::new();
    let mut all_token_saved: Vec<f64> = Vec::new();

    for repo in repos {
        let head = git_head_short(&repo);
        let archetypes = detect_archetypes(&repo);
        let query = choose_query(&archetypes);

        let mut tool_metrics: HashMap<String, ToolMetrics> = HashMap::new();
        for tool in &args.tools {
            let payload = match tool.as_str() {
                "meaning_pack" => serde_json::json!({
                    "path": repo.to_string_lossy(),
                    "query": query,
                    "max_chars": args.max_chars,
                    "auto_index": false,
                    "response_mode": args.response_mode,
                }),
                "atlas_pack" => serde_json::json!({
                    "path": repo.to_string_lossy(),
                    "query": query,
                    "max_chars": args.max_chars,
                    "response_mode": args.response_mode,
                }),
                other => {
                    tool_metrics.insert(
                        other.to_string(),
                        ToolMetrics {
                            ok: false,
                            error: Some(format!("unsupported tool: {other}")),
                            latency_ms: 0,
                            stable: false,
                            strict_ok: false,
                            strict_violations: vec!["unsupported_tool".to_string()],
                            area_entries: 0,
                            map_entries: 0,
                            map_noise_entries: 0,
                            noise_ratio: None,
                            output_area_entries: 0,
                            ev_slices: 0,
                            anchor_files: 0,
                            used_chars: 0,
                            baseline_chars: None,
                            token_saved: None,
                        },
                    );
                    continue;
                }
            };

            let started = Instant::now();
            let first = call_tool(&service, tool, payload.clone(), Duration::from_secs(30)).await;
            let latency_ms = started.elapsed().as_millis();
            all_latencies.push(latency_ms);

            let mut m = ToolMetrics {
                ok: false,
                error: None,
                latency_ms,
                stable: false,
                strict_ok: !args.strict,
                strict_violations: Vec::new(),
                area_entries: 0,
                map_entries: 0,
                map_noise_entries: 0,
                noise_ratio: None,
                output_area_entries: 0,
                ev_slices: 0,
                anchor_files: 0,
                used_chars: 0,
                baseline_chars: None,
                token_saved: None,
            };

            let first = match first {
                Ok(v) => v,
                Err(e) => {
                    m.error = Some(format!("{e:#}"));
                    if args.strict {
                        m.strict_ok = false;
                        m.strict_violations.push("tool_call_error".to_string());
                    }
                    if args.strict {
                        strict_failed = true;
                    }
                    tool_metrics.insert(tool.to_string(), m);
                    continue;
                }
            };
            if first.is_error == Some(true) {
                m.error = Some("tool returned error".to_string());
                if args.strict {
                    m.strict_ok = false;
                    m.strict_violations.push("tool_error".to_string());
                }
                if args.strict {
                    strict_failed = true;
                }
                tool_metrics.insert(tool.to_string(), m);
                continue;
            }

            let first_text = first
                .content
                .iter()
                .find_map(|c| c.as_text())
                .map(|t| t.text.as_str())
                .context("missing text output")?;
            let first_pack = extract_cp_pack(first_text)?;

            let second = call_tool(&service, tool, payload, Duration::from_secs(30)).await?;
            if second.is_error == Some(true) {
                m.error = Some("tool returned error (determinism call)".to_string());
                if args.strict {
                    m.strict_ok = false;
                    m.strict_violations
                        .push("tool_error_determinism".to_string());
                }
                if args.strict {
                    strict_failed = true;
                }
                tool_metrics.insert(tool.to_string(), m);
                continue;
            }
            let second_text = second
                .content
                .iter()
                .find_map(|c| c.as_text())
                .map(|t| t.text.as_str())
                .context("missing text output (determinism call)")?;
            let second_pack = extract_cp_pack(second_text)?;

            m.stable = first_pack == second_pack;

            let dict = parse_cp_dict(&first_pack)?;
            let map_stats = compute_map_stats(&first_pack, &dict);
            m.area_entries = map_stats.area_entries;
            m.output_area_entries = map_stats.output_area_entries;
            m.map_entries = map_stats.map_entries;
            m.map_noise_entries = map_stats.map_noise_entries;
            if map_stats.map_entries > 0 {
                let ratio = map_stats.map_noise_entries as f64 / map_stats.map_entries as f64;
                m.noise_ratio = Some(ratio);
                all_noise.push(ratio);
            }

            let tok = compute_token_saved(&repo, &first_pack, &dict)?;
            m.used_chars = tok.used_chars;
            m.ev_slices = tok.ev_slices;
            m.anchor_files = tok.anchor_files;
            m.baseline_chars = tok.baseline_chars;
            if let Some(saved) = tok.token_saved {
                m.token_saved = Some(saved);
                all_token_saved.push(saved);
            }

            if args.strict {
                if !m.stable {
                    m.strict_violations.push("unstable".to_string());
                }
                if m.latency_ms > args.strict_max_latency_ms as u128 {
                    m.strict_violations
                        .push(format!("latency_ms>{}", args.strict_max_latency_ms));
                }
                if let Some(ratio) = m.noise_ratio {
                    if ratio > args.strict_max_noise_ratio {
                        m.strict_violations
                            .push(format!("noise_ratio>{:.3}", args.strict_max_noise_ratio));
                    }
                }
                if let (Some(saved), Some(baseline)) = (m.token_saved, m.baseline_chars) {
                    if baseline >= args.strict_min_baseline_chars
                        && saved < args.strict_min_token_saved
                    {
                        m.strict_violations
                            .push(format!("token_saved<{:.3}", args.strict_min_token_saved));
                    }
                }
                m.strict_ok = m.strict_violations.is_empty();
                if !m.strict_ok {
                    strict_failed = true;
                }
            } else {
                m.strict_ok = true;
            }

            m.ok = true;
            tool_metrics.insert(tool.to_string(), m);
        }

        results.push(RepoResult {
            path: repo.to_string_lossy().to_string(),
            head,
            archetypes,
            tools: tool_metrics,
        });
    }

    // Compact stdout summary (still export full JSON/MD if requested).
    let mut lat_sorted = all_latencies.clone();
    lat_sorted.sort_unstable();
    let p50 = percentile(&lat_sorted, 50);
    let p95 = percentile(&lat_sorted, 95);
    let max = *lat_sorted.last().unwrap_or(&0);
    let mean_noise = if all_noise.is_empty() {
        0.0
    } else {
        all_noise.iter().sum::<f64>() / all_noise.len() as f64
    };
    let mean_saved = if all_token_saved.is_empty() {
        0.0
    } else {
        all_token_saved.iter().sum::<f64>() / all_token_saved.len() as f64
    };

    let strict_bad_calls = results
        .iter()
        .flat_map(|r| r.tools.values())
        .filter(|m| m.ok && !m.strict_ok)
        .count();
    if args.strict {
        eprintln!(
            "OK: repos={} tools={} latency_ms(p50={}, p95={}, max={}) noise_mean={:.4} token_saved_mean={:.4} strict_bad_calls={}",
            results.len(),
            args.tools.len(),
            p50,
            p95,
            max,
            mean_noise,
            mean_saved,
            strict_bad_calls
        );
    } else {
        eprintln!(
            "OK: repos={} tools={} latency_ms(p50={}, p95={}, max={}) noise_mean={:.4} token_saved_mean={:.4}",
            results.len(),
            args.tools.len(),
            p50,
            p95,
            max,
            mean_noise,
            mean_saved
        );
    }

    let report = ZooReport {
        root: args.root.to_string_lossy().to_string(),
        limit: args.limit,
        max_depth: args.max_depth,
        max_chars: args.max_chars,
        response_mode: args.response_mode,
        include_worktrees: args.include_worktrees,
        strict: args.strict,
        strict_policy: StrictPolicy {
            max_latency_ms: args.strict_max_latency_ms,
            max_noise_ratio: args.strict_max_noise_ratio,
            min_token_saved: args.strict_min_token_saved,
            min_baseline_chars: args.strict_min_baseline_chars,
        },
        repos: results,
    };

    if let Some(path) = args.out_json.as_ref() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let bytes = serde_json::to_vec_pretty(&report).context("serialize report json")?;
        std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
    }
    if let Some(path) = args.out_md.as_ref() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let md = render_md(&report);
        std::fs::write(path, md).with_context(|| format!("write {}", path.display()))?;
    }

    service.cancel().await.ok();
    if strict_failed {
        anyhow::bail!(
            "strict mode failed: at least one repo/tool did not meet stability requirements"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_file_slice_chars_is_binary_safe_and_bounded() -> Result<()> {
        let tmp = tempfile::tempdir().context("tempdir")?;
        let root = tmp.path();

        // 3 lines; middle has invalid UTF-8 bytes.
        let bytes: Vec<u8> = vec![
            b'a', b'\n', // 1
            0xff, 0xfe, b'\n', // 2
            b'c', b'\n', // 3
        ];
        std::fs::write(root.join("x.bin"), bytes).context("write x.bin")?;

        // Count lines 2..=3: (2 bytes + 1) + (1 byte + 1) = 5
        let got = count_file_slice_chars(root, "x.bin", 2, 3)?;
        anyhow::ensure!(got == 5, "expected 5, got {got}");
        Ok(())
    }

    #[test]
    fn noise_classifier_marks_worktrees_as_noise() {
        assert!(is_noise_map_dir(".worktrees"));
        assert!(is_noise_map_dir(".worktrees/foo"));
        assert!(is_noise_map_dir("./.worktrees/foo"));
    }
}
