use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum BoundaryKind {
    Cli,
    Http,
    Event,
    Env,
    Config,
    Db,
}

impl BoundaryKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            BoundaryKind::Cli => "cli",
            BoundaryKind::Http => "http",
            BoundaryKind::Event => "event",
            BoundaryKind::Env => "env",
            BoundaryKind::Config => "config",
            BoundaryKind::Db => "db",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct BoundaryCandidate {
    pub(super) kind: BoundaryKind,
    pub(super) file: String,
    pub(super) confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum EvidenceKind {
    Entrypoint,
    Contract,
    Boundary(BoundaryKind),
}

#[derive(Debug, Clone)]
pub(super) struct EvidenceItem {
    pub(super) kind: EvidenceKind,
    pub(super) file: String,
    pub(super) start_line: usize,
    pub(super) end_line: usize,
    pub(super) source_hash: Option<String>,
}

pub(super) fn directory_key(file_path: &str, depth: usize) -> String {
    let parts: Vec<&str> = file_path.split('/').collect();
    if parts.len() <= 1 {
        return ".".to_string();
    }
    let dir_parts = &parts[..parts.len() - 1];
    let depth = depth.min(dir_parts.len()).max(1);
    dir_parts[..depth].join("/")
}

pub(super) fn classify_files(files: &[String]) -> (Vec<String>, Vec<String>) {
    let mut entrypoints = Vec::new();
    let mut contracts = Vec::new();
    for file in files {
        let lc = file.to_ascii_lowercase();
        if is_entrypoint_candidate(&lc) {
            entrypoints.push(file.clone());
            continue;
        }
        if is_contract_candidate(&lc) {
            contracts.push(file.clone());
        }
    }
    entrypoints.sort();
    contracts.sort();
    (entrypoints, contracts)
}

pub(super) fn classify_boundaries(
    files: &[String],
    entrypoints: &[String],
    contracts: &[String],
) -> Vec<BoundaryCandidate> {
    let mut out: Vec<BoundaryCandidate> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();

    // Deterministic build/workspace configs (high-signal in onboarding).
    for file in files {
        let lc = file.to_ascii_lowercase();
        let kind = match lc.as_str() {
            "cargo.toml" | "package.json" | "pyproject.toml" | "go.mod" | "pom.xml"
            | "build.gradle" | "build.gradle.kts" | "makefile" | "justfile" => {
                Some(BoundaryKind::Config)
            }
            ".env.example" | ".env.sample" | ".env.template" | ".env.dist" => {
                Some(BoundaryKind::Env)
            }
            ".github/workflows/ci.yml" | ".github/workflows/ci.yaml" => Some(BoundaryKind::Config),
            _ => None,
        };
        let Some(kind) = kind else { continue };
        if !seen.insert(file.as_str()) {
            continue;
        }
        out.push(BoundaryCandidate {
            kind,
            file: file.clone(),
            confidence: 1.0,
        });
    }

    // Entrypoints are almost always a boundary, but the exact kind is heuristic.
    for file in entrypoints {
        let lc = file.to_ascii_lowercase();
        if !seen.insert(file.as_str()) {
            continue;
        }
        let (kind, confidence) =
            if lc.contains("/server.") || lc.contains("/api/") || lc.contains("/http/") {
                (BoundaryKind::Http, 0.7)
            } else if lc.contains("/cli/") || lc.contains("/cmd/") || lc.contains("/bin/") {
                (BoundaryKind::Cli, 0.7)
            } else {
                (BoundaryKind::Cli, 0.55)
            };
        out.push(BoundaryCandidate {
            kind,
            file: file.clone(),
            confidence,
        });
    }

    // If we have an OpenAPI contract, expose an HTTP boundary candidate even if entrypoint is unknown.
    if contracts.iter().any(|file| {
        let lc = file.to_ascii_lowercase();
        lc.ends_with("openapi.json")
            || lc.ends_with("openapi.yaml")
            || lc.ends_with("openapi.yml")
            || lc.contains("/openapi.")
    }) {
        // Best-effort: pick the first likely server entrypoint if present.
        if let Some(server) = entrypoints.iter().find(|file| {
            let lc = file.to_ascii_lowercase();
            lc.contains("server") || lc.contains("app")
        }) {
            if seen.insert(server.as_str()) {
                out.push(BoundaryCandidate {
                    kind: BoundaryKind::Http,
                    file: server.clone(),
                    confidence: 0.65,
                });
            }
        }
    }

    // DB boundary: detect common migration/schema layouts (path-only, safe).
    for file in files {
        let lc = file.to_ascii_lowercase();
        let is_db = lc.starts_with("migrations/")
            || lc.contains("/migrations/")
            || lc.ends_with("schema.sql")
            || lc.ends_with("schema.prisma")
            || lc.starts_with("prisma/");
        if !is_db {
            continue;
        }
        if !seen.insert(file.as_str()) {
            continue;
        }
        out.push(BoundaryCandidate {
            kind: BoundaryKind::Db,
            file: file.clone(),
            confidence: 0.85,
        });
    }

    // Event boundary: AsyncAPI and schema-like assets for message-driven systems.
    for file in files {
        let lc = file.to_ascii_lowercase();
        let is_event = lc == "asyncapi.yaml"
            || lc == "asyncapi.yml"
            || lc == "asyncapi.json"
            || lc.contains("/asyncapi.")
            || lc.ends_with(".avsc")
            || lc.starts_with("events/")
            || lc.contains("/events/")
            || lc.starts_with("schemas/events/")
            || lc.contains("/schemas/events/")
            || lc.starts_with("messages/")
            || lc.contains("/messages/");
        if !is_event {
            continue;
        }
        if !seen.insert(file.as_str()) {
            continue;
        }
        let confidence = if lc.contains("asyncapi") {
            1.0
        } else if lc.ends_with(".avsc") {
            0.9
        } else {
            0.75
        };
        out.push(BoundaryCandidate {
            kind: BoundaryKind::Event,
            file: file.clone(),
            confidence,
        });
    }

    // Stable ordering: kind priority, then path.
    out.sort_by(|a, b| {
        boundary_kind_rank(a.kind)
            .cmp(&boundary_kind_rank(b.kind))
            .then_with(|| a.file.cmp(&b.file))
    });
    out
}

fn boundary_kind_rank(kind: BoundaryKind) -> usize {
    match kind {
        BoundaryKind::Http => 0,
        BoundaryKind::Cli => 1,
        BoundaryKind::Event => 2,
        BoundaryKind::Env => 3,
        BoundaryKind::Config => 4,
        BoundaryKind::Db => 5,
    }
}

pub(super) fn is_entrypoint_candidate(file_lc: &str) -> bool {
    file_lc.ends_with("/src/main.rs")
        || file_lc.ends_with("/main.rs")
        || file_lc.ends_with("/main.py")
        || file_lc.ends_with("/app.py")
        || file_lc.ends_with("/server.py")
        || file_lc.ends_with("/index.js")
        || file_lc.ends_with("/server.js")
        || file_lc.ends_with("/main.ts")
        || file_lc.ends_with("/server.ts")
}

pub(super) fn is_contract_candidate(file_lc: &str) -> bool {
    file_lc.starts_with("contracts/")
        || file_lc.starts_with("proto/")
        || file_lc.contains("/openapi.")
        || file_lc.ends_with(".proto")
        || file_lc.ends_with(".schema.json")
        || file_lc.ends_with("openapi.json")
        || file_lc.ends_with("openapi.yaml")
        || file_lc.ends_with("openapi.yml")
        || file_lc.ends_with("asyncapi.json")
        || file_lc.ends_with("asyncapi.yaml")
        || file_lc.ends_with("asyncapi.yml")
        || file_lc.contains("/asyncapi.")
}

pub(super) fn contract_kind(file: &str) -> &'static str {
    let lc = file.to_ascii_lowercase();
    if lc.ends_with(".proto") || lc.starts_with("proto/") {
        return "proto";
    }
    if lc.ends_with(".schema.json") {
        return "jsonschema";
    }
    if lc.ends_with("openapi.json") || lc.ends_with("openapi.yaml") || lc.ends_with("openapi.yml") {
        return "openapi";
    }
    if lc.contains("/openapi.") {
        return "openapi";
    }
    if lc.ends_with("asyncapi.json")
        || lc.ends_with("asyncapi.yaml")
        || lc.ends_with("asyncapi.yml")
    {
        return "asyncapi";
    }
    if lc.contains("/asyncapi.") {
        return "asyncapi";
    }
    "contract"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FlowDirection {
    Publish,
    Subscribe,
}

impl FlowDirection {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            FlowDirection::Publish => "pub",
            FlowDirection::Subscribe => "sub",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct FlowEdge {
    pub(super) contract_file: String,
    pub(super) channel: String,
    pub(super) direction: FlowDirection,
    pub(super) protocol: Option<String>,
}

#[derive(Debug, Default)]
struct AsyncApiSummary {
    protocols: Vec<String>,
    channels: Vec<AsyncApiChannel>,
}

#[derive(Debug, Default)]
struct AsyncApiChannel {
    name: String,
    publish: bool,
    subscribe: bool,
}

pub(super) async fn extract_asyncapi_flows(root: &Path, contracts: &[String]) -> Vec<FlowEdge> {
    const MAX_READ_BYTES: usize = 256 * 1024;
    const MAX_CHANNELS: usize = 10;
    const MAX_PROTOCOLS: usize = 2;

    let mut out: Vec<FlowEdge> = Vec::new();
    for contract in contracts {
        if contract_kind(contract) != "asyncapi" {
            continue;
        }

        let Some(content) = read_file_prefix_utf8(root, contract, MAX_READ_BYTES).await else {
            continue;
        };
        let summary = extract_asyncapi_summary(&content);

        let protocol = summary.protocols.into_iter().take(MAX_PROTOCOLS).next();

        let mut channels = summary.channels;
        channels.sort_by(|a, b| a.name.cmp(&b.name));
        for ch in channels.into_iter().take(MAX_CHANNELS) {
            if ch.publish {
                out.push(FlowEdge {
                    contract_file: contract.clone(),
                    channel: ch.name.clone(),
                    direction: FlowDirection::Publish,
                    protocol: protocol.clone(),
                });
            }
            if ch.subscribe {
                out.push(FlowEdge {
                    contract_file: contract.clone(),
                    channel: ch.name.clone(),
                    direction: FlowDirection::Subscribe,
                    protocol: protocol.clone(),
                });
            }
        }
    }

    out.sort_by(|a, b| {
        a.contract_file
            .cmp(&b.contract_file)
            .then_with(|| a.channel.cmp(&b.channel))
            .then_with(|| (a.direction.as_str()).cmp(b.direction.as_str()))
    });
    out
}

async fn read_file_prefix_utf8(root: &Path, rel: &str, max_bytes: usize) -> Option<String> {
    let abs = root.join(rel);
    let mut file = File::open(abs).await.ok()?;
    let mut buf = vec![0u8; max_bytes];
    let n = file.read(&mut buf).await.ok()?;
    buf.truncate(n);
    String::from_utf8(buf).ok()
}

fn extract_asyncapi_summary(content: &str) -> AsyncApiSummary {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(content) {
        return extract_asyncapi_summary_json(&json);
    }
    extract_asyncapi_summary_yaml_like(content)
}

fn extract_asyncapi_summary_json(value: &serde_json::Value) -> AsyncApiSummary {
    let mut out = AsyncApiSummary::default();

    if let Some(servers) = value.get("servers").and_then(|v| v.as_object()) {
        for server in servers.values() {
            if let Some(protocol) = server.get("protocol").and_then(|v| v.as_str()) {
                let protocol = protocol.trim().to_ascii_lowercase();
                if protocol.is_empty() {
                    continue;
                }
                if !out.protocols.iter().any(|p| p == &protocol) {
                    out.protocols.push(protocol);
                }
            }
        }
    }

    if let Some(channels) = value.get("channels").and_then(|v| v.as_object()) {
        for (name, channel) in channels {
            let publish = channel.get("publish").is_some();
            let subscribe = channel.get("subscribe").is_some();
            out.channels.push(AsyncApiChannel {
                name: name.clone(),
                publish,
                subscribe,
            });
        }
    }

    out
}

fn extract_asyncapi_summary_yaml_like(content: &str) -> AsyncApiSummary {
    let mut out = AsyncApiSummary::default();

    // Best-effort protocol detection: look for `protocol: <value>` lines.
    for raw in content.lines().take(5000) {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("protocol:") else {
            continue;
        };
        let protocol = rest.trim().trim_matches('"').trim_matches('\'');
        if protocol.is_empty() {
            continue;
        }
        let protocol = protocol.to_ascii_lowercase();
        if !out.protocols.iter().any(|p| p == &protocol) {
            out.protocols.push(protocol);
        }
    }

    // Best-effort channel extraction from YAML:
    // channels:
    //   topic.name:
    //     publish:
    //     subscribe:
    let lines: Vec<&str> = content.lines().collect();
    let mut idx = 0usize;
    while idx < lines.len() {
        let raw = lines[idx];
        if raw.trim_start().starts_with("channels:") {
            break;
        }
        idx += 1;
    }
    if idx >= lines.len() {
        return out;
    }

    let channels_indent = count_leading_spaces(lines[idx]);
    idx += 1;

    let mut current: Option<AsyncApiChannel> = None;
    let mut current_indent: usize = 0;

    while idx < lines.len() {
        let raw = lines[idx];
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            idx += 1;
            continue;
        }
        let indent = count_leading_spaces(raw);
        if indent <= channels_indent {
            break;
        }

        if trimmed.ends_with(':') && !trimmed.starts_with('-') {
            let key = trimmed.trim_end_matches(':').trim();
            let key = key.trim_matches('"').trim_matches('\'');
            if !key.is_empty() && key != "publish" && key != "subscribe" {
                if let Some(ch) = current.take() {
                    out.channels.push(ch);
                }
                current_indent = indent;
                current = Some(AsyncApiChannel {
                    name: key.to_string(),
                    publish: false,
                    subscribe: false,
                });
                idx += 1;
                continue;
            }
        }

        if let Some(ch) = current.as_mut() {
            if indent > current_indent {
                if trimmed.starts_with("publish:") {
                    ch.publish = true;
                } else if trimmed.starts_with("subscribe:") {
                    ch.subscribe = true;
                }
            }
        }

        idx += 1;
    }

    if let Some(ch) = current.take() {
        out.channels.push(ch);
    }

    out
}

fn count_leading_spaces(s: &str) -> usize {
    s.as_bytes().iter().take_while(|&&b| b == b' ').count()
}

pub(super) fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<invalid>\"".to_string())
}

pub(super) async fn hash_and_count_lines(path: &Path) -> Result<(String, usize)> {
    let meta = tokio::fs::metadata(path).await?;
    let file_size = meta.len();

    let mut file = File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut newlines = 0usize;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        newlines += buf[..n].iter().filter(|&&b| b == b'\n').count();
    }
    let hash = format!("{:x}", hasher.finalize());
    let lines = if file_size == 0 { 0 } else { newlines + 1 };
    Ok((hash, lines))
}

pub(super) fn shrink_pack(pack: &mut String) -> bool {
    // Deterministic shrink while preserving the last `NBA ...` line when present.
    let trimmed = pack.trim_end_matches('\n');
    if trimmed.is_empty() {
        return false;
    }

    let last_line_start = trimmed.rfind('\n').map(|pos| pos + 1).unwrap_or(0);
    let last_line = &trimmed[last_line_start..];
    let is_nba = last_line.starts_with("NBA ");

    if !is_nba {
        if last_line_start < 10 {
            return false;
        }
        pack.truncate(last_line_start);
        return true;
    }

    // Keep NBA, drop the line right before it.
    if last_line_start == 0 {
        return false;
    }
    let before_last = &trimmed[..last_line_start - 1];
    let Some(prev_start) = before_last.rfind('\n').map(|pos| pos + 1) else {
        return false;
    };
    if prev_start < 10 {
        return false;
    }
    let mut rebuilt = String::new();
    rebuilt.push_str(&trimmed[..prev_start]);
    rebuilt.push_str(last_line);
    rebuilt.push('\n');
    *pack = rebuilt;
    true
}

#[derive(Default)]
pub(super) struct CognitivePack {
    dict: Vec<String>,
    dict_index: BTreeMap<String, usize>,
    lines: Vec<String>,
}

impl CognitivePack {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn dict_intern(&mut self, value: String) {
        if self.dict_index.contains_key(&value) {
            return;
        }
        let idx = self.dict.len();
        self.dict.push(value.clone());
        self.dict_index.insert(value, idx);
    }

    pub(super) fn dict_id(&self, value: &str) -> String {
        let idx = *self
            .dict_index
            .get(value)
            .unwrap_or_else(|| panic!("missing dict entry for {value}"));
        format!("d{idx}")
    }

    pub(super) fn push_line(&mut self, line: &str) {
        self.lines.push(line.to_string());
    }

    pub(super) fn render(&self) -> String {
        if self.dict.is_empty() {
            return self.lines.join("\n") + "\n";
        }

        let mut out = String::new();
        let base_lines = self.lines.iter().map(String::as_str).collect::<Vec<_>>();
        let insert_at = base_lines.len().min(3);
        for (idx, line) in base_lines.iter().enumerate() {
            if idx == insert_at {
                out.push_str("S DICT\n");
                for (d_idx, value) in self.dict.iter().enumerate() {
                    out.push_str(&format!("D d{d_idx} {}\n", json_string(value)));
                }
            }
            out.push_str(line);
            out.push('\n');
        }
        if insert_at == base_lines.len() {
            out.push_str("S DICT\n");
            for (d_idx, value) in self.dict.iter().enumerate() {
                out.push_str(&format!("D d{d_idx} {}\n", json_string(value)));
            }
        }
        out
    }
}

pub(super) fn build_ev_file_index(evidence: &[EvidenceItem]) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for (idx, ev) in evidence.iter().enumerate() {
        out.entry(ev.file.clone())
            .or_insert_with(|| format!("ev{idx}"));
    }
    out
}
