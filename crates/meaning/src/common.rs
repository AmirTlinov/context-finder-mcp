use anyhow::Result;
use context_code_chunker::{Chunker, ChunkerConfig};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum AnchorKind {
    Canon,
    HowTo,
    Infra,
    Contract,
    Entrypoint,
    Artifact,
    Experiment,
}

impl AnchorKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            AnchorKind::Canon => "canon",
            AnchorKind::HowTo => "howto",
            AnchorKind::Infra => "infra",
            AnchorKind::Contract => "contract",
            AnchorKind::Entrypoint => "entrypoint",
            AnchorKind::Artifact => "artifact",
            AnchorKind::Experiment => "experiment",
        }
    }
}

// Artifact stores are huge by definition. We treat them as "meaning" (anchors) instead of
// letting them dominate structural maps.
const ARTIFACT_STORE_SCOPES: &[&str] = &[
    "artifacts",
    "artifact",
    "results",
    "runs",
    "outputs",
    "output",
    "checkpoints",
    "checkpoint",
];

pub(super) fn is_artifact_scope(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or("").trim();
    if first.is_empty() || first == "." {
        return false;
    }
    let lowered = first.to_ascii_lowercase();
    ARTIFACT_STORE_SCOPES
        .iter()
        .any(|candidate| *candidate == lowered)
}

pub(super) fn artifact_scope_rank(scope: &str) -> usize {
    match scope {
        "artifacts" | "artifact" => 0,
        "results" => 1,
        "runs" => 2,
        "outputs" | "output" => 3,
        "checkpoints" | "checkpoint" => 4,
        _ => 10,
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
    Anchor(AnchorKind),
}

#[derive(Debug, Clone)]
pub(super) struct OutlineSymbol {
    pub(super) kind: &'static str,
    pub(super) name: String,
    pub(super) start_line: usize,
    pub(super) end_line: usize,
    pub(super) confidence: f32,
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
    let mut entrypoints: Vec<(usize, String)> = Vec::new();
    let mut contracts = Vec::new();
    for file in files {
        let lc = file.to_ascii_lowercase();
        if is_artifact_scope(&lc) {
            continue;
        }
        if let Some(rank) = entrypoint_rank(&lc) {
            entrypoints.push((rank, file.clone()));
            continue;
        }
        if is_contract_candidate(&lc) {
            contracts.push(file.clone());
        }
    }
    entrypoints.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let entrypoints = entrypoints.into_iter().map(|(_, file)| file).collect();
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

    // Infra boundary: highlight k8s/helm/terraform layouts (path-only, safe).
    //
    // We intentionally keep this tight to avoid flooding boundaries for repos with many manifests.
    let mut infra_candidates: Vec<(usize, &String, f32)> = Vec::new();
    for file in files {
        let lc = file.to_ascii_lowercase();
        let basename = lc.rsplit('/').next().unwrap_or(lc.as_str());
        let is_yaml = lc.ends_with(".yaml") || lc.ends_with(".yml");
        let is_tf = lc.ends_with(".tf") || lc.ends_with(".tfvars") || lc.ends_with(".hcl");
        let is_tiltfile = basename == "tiltfile" && lc == "tiltfile";

        let is_k8s_dir = lc.starts_with("k8s/")
            || lc.contains("/k8s/")
            || lc.starts_with("kubernetes/")
            || lc.contains("/kubernetes/")
            || lc.starts_with("manifests/")
            || lc.contains("/manifests/")
            || lc.starts_with("deploy/")
            || lc.contains("/deploy/")
            || lc.starts_with("kustomize/")
            || lc.contains("/kustomize/");
        let is_helm_dir = lc.starts_with("charts/")
            || lc.contains("/charts/")
            || lc.contains("/helm/")
            || basename == "chart.yaml"
            || basename == "values.yaml"
            || basename == "values.yml"
            || basename == "helmfile.yaml"
            || basename == "helmfile.yml"
            || basename == "helmrelease.yaml"
            || basename == "helmrelease.yml";
        let is_gitops_dir = lc.starts_with("argocd/")
            || lc.contains("/argocd/")
            || lc.starts_with("argo/")
            || lc.contains("/argo/")
            || lc.starts_with("flux/")
            || lc.contains("/flux/")
            || lc.starts_with("gitops/")
            || lc.contains("/gitops/")
            || lc.starts_with("clusters/")
            || lc.contains("/clusters/");
        let is_tf_dir = lc.starts_with("terraform/")
            || lc.contains("/terraform/")
            || lc.starts_with("infra/")
            || lc.contains("/infra/");
        let is_tf_root_candidate = matches!(
            basename,
            "main.tf"
                | "variables.tf"
                | "versions.tf"
                | "provider.tf"
                | "providers.tf"
                | "backend.tf"
                | "outputs.tf"
                | "terraform.tf"
                | "terragrunt.hcl"
        );
        let is_infra_yaml = is_k8s_dir
            || is_helm_dir
            || is_gitops_dir
            || matches!(
                basename,
                "chart.yaml"
                    | "values.yaml"
                    | "values.yml"
                    | "helmfile.yaml"
                    | "helmfile.yml"
                    | "helmrelease.yaml"
                    | "helmrelease.yml"
                    | "kustomization.yaml"
                    | "kustomization.yml"
                    | "skaffold.yaml"
                    | "skaffold.yml"
                    | "werf.yaml"
                    | "werf.yml"
                    | "devspace.yaml"
                    | "devspace.yml"
            );

        if !(is_yaml || is_tf || is_tiltfile) {
            continue;
        }
        if is_yaml && !is_infra_yaml {
            continue;
        }
        if is_tf && !(is_tf_dir || is_tf_root_candidate || basename == "terragrunt.hcl") {
            continue;
        }

        // Stable ranking: prefer canonical infra entry files.
        let (rank, confidence) = if basename == "chart.yaml" {
            (0usize, 0.9f32)
        } else if basename == "values.yaml" || basename == "values.yml" {
            (1usize, 0.85f32)
        } else if basename == "helmfile.yaml" || basename == "helmfile.yml" {
            (2usize, 0.82f32)
        } else if basename == "helmrelease.yaml" || basename == "helmrelease.yml" {
            (3usize, 0.83f32)
        } else if basename == "kustomization.yaml" || basename == "kustomization.yml" {
            (4usize, 0.85f32)
        } else if is_gitops_dir
            && matches!(
                basename,
                "application.yaml"
                    | "application.yml"
                    | "applicationset.yaml"
                    | "applicationset.yml"
            )
        {
            (5usize, 0.82f32)
        } else if basename == "terragrunt.hcl" {
            (6usize, 0.85f32)
        } else if basename == "skaffold.yaml" || basename == "skaffold.yml" {
            (7usize, 0.83f32)
        } else if is_tiltfile {
            (8usize, 0.83f32)
        } else if basename == "werf.yaml" || basename == "werf.yml" {
            (9usize, 0.82f32)
        } else if basename == "devspace.yaml" || basename == "devspace.yml" {
            (10usize, 0.82f32)
        } else if lc.contains("ingress") {
            (11usize, 0.8f32)
        } else if lc.contains("service") {
            (12usize, 0.78f32)
        } else if lc.contains("deployment") || lc.contains("statefulset") {
            (13usize, 0.78f32)
        } else if basename == "main.tf" {
            (14usize, 0.85f32)
        } else if basename == "variables.tf" {
            (15usize, 0.82f32)
        } else if matches!(
            basename,
            "versions.tf"
                | "provider.tf"
                | "providers.tf"
                | "backend.tf"
                | "outputs.tf"
                | "terraform.tf"
        ) {
            (16usize, 0.8f32)
        } else if is_tf {
            (17usize, 0.78f32)
        } else {
            (18usize, 0.75f32)
        };

        infra_candidates.push((rank, file, confidence));
    }
    infra_candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    infra_candidates.truncate(8);
    for (_, file, confidence) in infra_candidates {
        if !seen.insert(file.as_str()) {
            continue;
        }
        out.push(BoundaryCandidate {
            kind: BoundaryKind::Config,
            file: file.clone(),
            confidence,
        });
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

pub(super) fn entrypoint_rank(file_lc: &str) -> Option<usize> {
    // The goal is “best file to start reading the code”, not only “executable main”.
    // Ranking is critical: large workspaces may contain many `lib.rs` files; those must not
    // crowd out real entrypoints like `main.rs` / `main.py`.

    // Highest-signal: obvious executables at the root.
    if matches!(
        file_lc,
        "main.rs"
            | "main.py"
            | "__main__.py"
            | "app.py"
            | "server.py"
            | "index.js"
            | "server.js"
            | "main.ts"
            | "server.ts"
    ) {
        return Some(0);
    }

    // Common “src root” mains.
    if matches!(
        file_lc,
        "src/main.rs"
            | "src/main.py"
            | "src/__main__.py"
            | "src/app.py"
            | "src/server.py"
            | "src/index.js"
            | "src/index.ts"
    ) {
        return Some(1);
    }

    // Nested mains (monorepos / multi-crate layouts): `*/src/main.*` etc.
    if file_lc.ends_with("/src/main.rs")
        || file_lc.ends_with("/src/main.py")
        || file_lc.ends_with("/src/__main__.py")
        || file_lc.ends_with("/src/app.py")
        || file_lc.ends_with("/src/server.py")
        || file_lc.ends_with("/src/index.js")
        || file_lc.ends_with("/src/index.ts")
    {
        return Some(2);
    }

    // Go conventions: `cmd/<name>/main.go` (repo-root `main.go` is also common).
    if file_lc == "main.go" || (file_lc.starts_with("cmd/") && file_lc.ends_with("/main.go")) {
        return Some(2);
    }

    // Rust library roots are useful for “read the core”, but should be ranked below mains.
    if matches!(file_lc, "lib.rs" | "src/lib.rs") || file_lc.ends_with("/src/lib.rs") {
        return Some(3);
    }

    // Python packages: prefer shallow `src/<pkg>/__init__.py` as a library root.
    if file_lc == "src/__init__.py" {
        return Some(3);
    }
    if file_lc.starts_with("src/")
        && file_lc.ends_with("/__init__.py")
        && file_lc.split('/').count() == 3
    {
        return Some(4);
    }

    None
}

pub(super) fn is_contract_candidate(file_lc: &str) -> bool {
    let lc = file_lc.trim();
    if lc.is_empty() {
        return false;
    }

    let basename = lc.rsplit('/').next().unwrap_or(lc);
    let is_contract_like_ext = lc.ends_with(".proto")
        || lc.ends_with(".avsc")
        || lc.ends_with(".yaml")
        || lc.ends_with(".yml")
        || lc.ends_with(".json")
        || lc.ends_with(".toml")
        || lc.ends_with(".md")
        || lc.ends_with(".rst")
        || lc.ends_with(".txt");
    let is_contract_dir = lc.starts_with("contracts/")
        || lc.starts_with("proto/")
        || lc.starts_with("docs/contracts/")
        || lc.starts_with("docs/contract/")
        || lc.starts_with("docs/spec/")
        || lc.starts_with("docs/specs/")
        || lc.starts_with("docs/protocol/")
        || lc.starts_with("docs/protocols/")
        || lc.starts_with("schemas/")
        || lc.starts_with("schema/")
        || lc.starts_with("spec/")
        || lc.starts_with("specs/")
        || lc.starts_with("protocol/")
        || lc.starts_with("protocols/")
        || lc.contains("/contracts/")
        || lc.contains("/contract/")
        || lc.contains("/schemas/")
        || lc.contains("/schema/")
        || lc.contains("/specs/")
        || lc.contains("/spec/")
        || lc.contains("/protocols/")
        || lc.contains("/protocol/");

    if is_contract_dir && is_contract_like_ext {
        // Avoid treating “docs/contracts/…” or “protocols/…” as contracts when they are just
        // directory index files without any spec-like content.
        if matches!(
            basename,
            "readme.md" | "readme.rst" | "readme.txt" | "index.md"
        ) {
            return true;
        }
        return true;
    }

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

    let mut out: Vec<FlowEdge> = Vec::new();
    for contract in contracts {
        if contract_kind(contract) != "asyncapi" {
            continue;
        }

        let Some(content) = read_file_prefix_utf8(root, contract, MAX_READ_BYTES).await else {
            continue;
        };
        let summary = extract_asyncapi_summary(&content);

        let protocol = summary.protocols.into_iter().next();

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

pub(super) async fn read_file_prefix_utf8(
    root: &Path,
    rel: &str,
    max_bytes: usize,
) -> Option<String> {
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

pub(super) async fn detect_channel_mentions(
    root: &Path,
    files: &[String],
    channels: &[String],
) -> HashMap<String, String> {
    const MAX_SCAN_FILES: usize = 200;
    const MAX_READ_BYTES: usize = 64 * 1024;
    const MAX_CHANNELS: usize = 20;

    let mut wanted: Vec<String> = channels.to_vec();
    wanted.sort();
    wanted.dedup();
    wanted.truncate(MAX_CHANNELS);

    let mut out: HashMap<String, String> = HashMap::new();
    if wanted.is_empty() {
        return out;
    }

    let mut candidates: Vec<&String> = files
        .iter()
        .filter(|file| is_code_file_candidate(&file.to_ascii_lowercase()))
        .collect();
    candidates.sort();

    for file in candidates.into_iter().take(MAX_SCAN_FILES) {
        if out.len() >= wanted.len() {
            break;
        }
        let Some(content) = read_file_prefix_utf8(root, file, MAX_READ_BYTES).await else {
            continue;
        };
        for channel in &wanted {
            if out.contains_key(channel) {
                continue;
            }
            if content.contains(channel) {
                out.insert(channel.clone(), file.clone());
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
pub(super) struct BrokerCandidate {
    pub(super) proto: String,
    pub(super) file: String,
    pub(super) confidence: f32,
}

pub(super) async fn detect_brokers(
    root: &Path,
    files: &[String],
    flows: &[FlowEdge],
) -> Vec<BrokerCandidate> {
    const MAX_CANDIDATE_FILES: usize = 30;
    const MAX_READ_BYTES: usize = 192 * 1024;
    const MAX_BROKERS: usize = 4;

    let mut wanted: Vec<String> = flows
        .iter()
        .filter_map(|f| f.protocol.as_ref())
        .map(|p| p.to_ascii_lowercase())
        .collect();
    wanted.sort();
    wanted.dedup();
    if wanted.is_empty() {
        wanted = vec!["kafka", "nats", "amqp", "mqtt", "pulsar"]
            .into_iter()
            .map(|s| s.to_string())
            .collect();
    }

    let mut candidates: Vec<&String> = files
        .iter()
        .filter(|file| is_broker_config_candidate(&file.to_ascii_lowercase()))
        .collect();
    candidates.sort();

    let mut out: Vec<BrokerCandidate> = Vec::new();
    let mut seen_files: HashSet<&str> = HashSet::new();

    for file in candidates.into_iter().take(MAX_CANDIDATE_FILES) {
        if out.len() >= MAX_BROKERS {
            break;
        }
        if !seen_files.insert(file.as_str()) {
            continue;
        }
        let Some(content) = read_file_prefix_utf8(root, file, MAX_READ_BYTES).await else {
            continue;
        };
        let content_lc = content.to_ascii_lowercase();
        for proto in &wanted {
            if !content_mentions_proto(&content_lc, proto) {
                continue;
            }
            let mut confidence = 0.75;
            if file.to_ascii_lowercase().contains("docker-compose")
                || file.to_ascii_lowercase().ends_with("compose.yml")
                || file.to_ascii_lowercase().ends_with("compose.yaml")
            {
                confidence = 0.9;
            } else if content_lc.contains("image:") {
                confidence = 0.85;
            }
            out.push(BrokerCandidate {
                proto: proto.clone(),
                file: file.clone(),
                confidence,
            });
            break;
        }
    }

    out.sort_by(|a, b| {
        b.confidence
            .total_cmp(&a.confidence)
            .then_with(|| a.proto.cmp(&b.proto))
            .then_with(|| a.file.cmp(&b.file))
    });
    out.truncate(MAX_BROKERS);
    out
}

fn is_code_file_candidate(file_lc: &str) -> bool {
    if file_lc.starts_with("target/")
        || file_lc.contains("/target/")
        || file_lc.starts_with("node_modules/")
        || file_lc.contains("/node_modules/")
        || file_lc.starts_with("vendor/")
        || file_lc.contains("/vendor/")
        || file_lc.starts_with(".git/")
        || file_lc.contains("/.git/")
    {
        return false;
    }
    file_lc.ends_with(".rs")
        || file_lc.ends_with(".go")
        || file_lc.ends_with(".py")
        || file_lc.ends_with(".js")
        || file_lc.ends_with(".ts")
        || file_lc.ends_with(".java")
        || file_lc.ends_with(".kt")
        || file_lc.ends_with(".kts")
        || file_lc.ends_with(".cs")
        || file_lc.ends_with(".cpp")
        || file_lc.ends_with(".c")
        || file_lc.ends_with(".h")
        || file_lc.ends_with(".hpp")
}

fn is_broker_config_candidate(file_lc: &str) -> bool {
    let is_compose = file_lc.ends_with("docker-compose.yml")
        || file_lc.ends_with("docker-compose.yaml")
        || file_lc.ends_with("compose.yml")
        || file_lc.ends_with("compose.yaml");
    if is_compose {
        return true;
    }

    let basename = file_lc.rsplit('/').next().unwrap_or(file_lc);
    if basename == "tiltfile" && file_lc == basename {
        return true;
    }

    let is_tf =
        file_lc.ends_with(".tf") || file_lc.ends_with(".tfvars") || file_lc.ends_with(".hcl");
    if is_tf {
        let is_tf_dir = file_lc.starts_with("terraform/")
            || file_lc.contains("/terraform/")
            || file_lc.starts_with("infra/")
            || file_lc.contains("/infra/");
        let is_tf_root_candidate = matches!(
            basename,
            "main.tf"
                | "variables.tf"
                | "versions.tf"
                | "provider.tf"
                | "providers.tf"
                | "backend.tf"
                | "outputs.tf"
                | "terraform.tf"
                | "terragrunt.hcl"
        );
        let is_root = file_lc == basename;
        return is_tf_dir || (is_root && is_tf_root_candidate) || basename == "terragrunt.hcl";
    }

    let is_infra_dir = file_lc.starts_with("k8s/")
        || file_lc.contains("/k8s/")
        || file_lc.starts_with("kubernetes/")
        || file_lc.contains("/kubernetes/")
        || file_lc.starts_with("manifests/")
        || file_lc.contains("/manifests/")
        || file_lc.starts_with("deploy/")
        || file_lc.contains("/deploy/")
        || file_lc.starts_with("kustomize/")
        || file_lc.contains("/kustomize/")
        || file_lc.starts_with("infra/")
        || file_lc.contains("/infra/")
        || file_lc.starts_with("charts/")
        || file_lc.contains("/charts/")
        || file_lc.contains("/helm/")
        || file_lc.starts_with("argocd/")
        || file_lc.contains("/argocd/")
        || file_lc.starts_with("argo/")
        || file_lc.contains("/argo/")
        || file_lc.starts_with("flux/")
        || file_lc.contains("/flux/")
        || file_lc.starts_with("gitops/")
        || file_lc.contains("/gitops/")
        || file_lc.starts_with("clusters/")
        || file_lc.contains("/clusters/")
        || matches!(
            basename,
            "helmfile.yaml"
                | "helmfile.yml"
                | "helmrelease.yaml"
                | "helmrelease.yml"
                | "kustomization.yaml"
                | "kustomization.yml"
                | "skaffold.yaml"
                | "skaffold.yml"
                | "werf.yaml"
                | "werf.yml"
                | "devspace.yaml"
                | "devspace.yml"
        );
    if !is_infra_dir {
        return false;
    }

    file_lc.ends_with(".yaml") || file_lc.ends_with(".yml")
}

fn content_mentions_proto(content_lc: &str, proto_lc: &str) -> bool {
    match proto_lc {
        "kafka" => {
            content_lc.contains("kafka")
                || content_lc.contains("cp-kafka")
                || content_lc.contains("confluentinc")
                || content_lc.contains("bitnami/kafka")
        }
        "nats" => content_lc.contains("nats") || content_lc.contains("natsio"),
        "amqp" | "rabbitmq" => content_lc.contains("rabbitmq") || content_lc.contains("amqp"),
        "mqtt" => content_lc.contains("mqtt"),
        "pulsar" => content_lc.contains("pulsar"),
        other => content_lc.contains(other),
    }
}

pub(super) fn infer_actor_by_path(reference_file: &str, entrypoints: &[String]) -> Option<String> {
    let (reference_dir, _) = reference_file.rsplit_once('/')?;
    if reference_dir.is_empty() {
        return None;
    }

    let mut best: Option<&String> = None;
    let mut best_score: usize = 0;
    for ep in entrypoints {
        let ep_dir = ep.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let score = common_prefix_segments(reference_dir, ep_dir);
        if score == 0 {
            continue;
        }
        if score > best_score {
            best = Some(ep);
            best_score = score;
            continue;
        }
        if score == best_score {
            if let Some(current) = best {
                if ep < current {
                    best = Some(ep);
                }
            }
        }
    }
    best.cloned()
}

pub(super) fn infer_flow_actor(contract_file: &str, entrypoints: &[String]) -> Option<String> {
    if entrypoints.is_empty() {
        return None;
    }
    // Root-level AsyncAPI: safe only when the repo clearly has a single entrypoint.
    if !contract_file.contains('/') {
        return (entrypoints.len() == 1).then(|| entrypoints[0].clone());
    }
    infer_actor_by_path(contract_file, entrypoints)
}

fn common_prefix_segments(a: &str, b: &str) -> usize {
    a.split('/')
        .filter(|p| !p.is_empty())
        .zip(b.split('/').filter(|p| !p.is_empty()))
        .take_while(|(x, y)| x == y)
        .count()
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
    let mut lines: Vec<String> = pack
        .lines()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect();
    if lines.is_empty() {
        return false;
    }

    if lines.first().map(|line| line.as_str()) != Some("CPV1") {
        return shrink_pack_simple(pack);
    }
    let Some(nba_idx) = lines.iter().rposition(|line| line.starts_with("NBA ")) else {
        return shrink_pack_simple(pack);
    };
    if nba_idx == 0 {
        return false;
    }

    let mut changed = remove_one_low_priority_body_line(&mut lines, nba_idx);
    changed |= prune_unused_ev_lines(&mut lines);
    changed |= prune_unused_dict_lines(&mut lines);
    changed |= remove_empty_sections(&mut lines);

    // Last-resort: fail-soft to a minimal CP that stays parseable and deterministic.
    if !changed {
        let mut minimal: Vec<String> = Vec::new();
        if let Some(first) = lines.first() {
            minimal.push(first.clone());
        }
        if let Some(root_fp) = lines.iter().find(|line| line.starts_with("ROOT_FP ")) {
            minimal.push(root_fp.clone());
        }
        if let Some(query) = lines.iter().find(|line| line.starts_with("QUERY ")) {
            minimal.push(query.clone());
        }

        // Prefer preserving at least one evidence pointer for “precision fetch”, even under
        // extreme budgets. This keeps the pack actionable (semantic zoom → exact read).
        if let Some(ev_line) = lines.iter().find(|line| line.starts_with("EV ")) {
            let ev_id = ev_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("ev0")
                .to_string();
            let file_id = ev_line
                .split_whitespace()
                .find_map(|token| token.strip_prefix("file="))
                .map(str::to_string);
            let range = ev_line
                .split_whitespace()
                .find(|token| token.starts_with('L') && token.contains("-L"))
                .unwrap_or("L1-L1")
                .to_string();

            if let Some(file_id) = file_id {
                let dict_prefix = format!("D {file_id} ");
                if let Some(d_line) = lines.iter().find(|line| line.starts_with(&dict_prefix)) {
                    minimal.push("S DICT".to_string());
                    minimal.push(d_line.clone());
                    minimal.push("S EVIDENCE".to_string());
                    minimal.push(ev_line.clone());
                    minimal.push(format!(
                        "NBA evidence_fetch ev={ev_id} file={file_id} {range}"
                    ));
                    *pack = minimal.join("\n") + "\n";
                    return true;
                }
            }
        }

        minimal.push("NBA map".to_string());
        *pack = minimal.join("\n") + "\n";
        return true;
    }

    *pack = lines.join("\n") + "\n";
    true
}

fn shrink_pack_simple(pack: &mut String) -> bool {
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

fn remove_one_low_priority_body_line(lines: &mut Vec<String>, nba_idx: usize) -> bool {
    // Keep this deterministic: lowest-signal content is removed first.
    // Note: we intentionally do *not* remove `D ...`, `EV ...`, or headers here.
    #[derive(Clone, Copy)]
    struct PrefixPolicy {
        prefix: &'static str,
        min_keep: usize,
    }

    // This is a “graceful degradation” policy: trim *counts* first, preserve claim diversity.
    // The most valuable navigation primitives are kept longer:
    // - `ANCHOR` (where to start), `STEP` (how to run), `AREA` (sense map).
    // - Under extreme budgets we may still fall back to the minimal CP (see shrink_pack()).
    const POLICIES: [PrefixPolicy; 10] = [
        PrefixPolicy {
            prefix: "MAP ",
            min_keep: 0,
        },
        PrefixPolicy {
            prefix: "SYM ",
            min_keep: 0,
        },
        PrefixPolicy {
            prefix: "FLOW ",
            min_keep: 0,
        },
        PrefixPolicy {
            prefix: "BROKER ",
            min_keep: 0,
        },
        PrefixPolicy {
            prefix: "ENTRY ",
            min_keep: 0,
        },
        PrefixPolicy {
            prefix: "CONTRACT ",
            min_keep: 0,
        },
        PrefixPolicy {
            prefix: "BOUNDARY ",
            min_keep: 0,
        },
        PrefixPolicy {
            prefix: "STEP ",
            min_keep: 1,
        },
        PrefixPolicy {
            prefix: "AREA ",
            min_keep: 1,
        },
        PrefixPolicy {
            prefix: "ANCHOR ",
            min_keep: 3,
        },
    ];

    for policy in POLICIES {
        let count = lines
            .iter()
            .take(nba_idx)
            .filter(|line| line.starts_with(policy.prefix))
            .count();
        if count <= policy.min_keep {
            continue;
        }
        if let Some(idx) = lines
            .iter()
            .take(nba_idx)
            .rposition(|line| line.starts_with(policy.prefix))
        {
            lines.remove(idx);
            return true;
        }
    }

    false
}

fn remove_empty_sections(lines: &mut Vec<String>) -> bool {
    let mut changed = false;
    let mut idx = 0usize;
    while idx < lines.len() {
        if !lines[idx].starts_with("S ") {
            idx += 1;
            continue;
        }

        let start = idx;
        let mut end = start + 1;
        while end < lines.len() && !lines[end].starts_with("S ") && !lines[end].starts_with("NBA ")
        {
            end += 1;
        }
        let has_data = (start + 1) < end;
        if !has_data {
            lines.remove(start);
            changed = true;
            continue;
        }
        idx = end;
    }
    changed
}

fn prune_unused_ev_lines(lines: &mut Vec<String>) -> bool {
    let mut used: HashSet<String> = HashSet::new();
    for line in lines.iter().filter(|line| !line.starts_with("EV ")) {
        for token in line.split_whitespace() {
            if let Some(ev) = token.strip_prefix("ev=") {
                used.insert(ev.to_string());
            }
        }
    }

    let mut changed = false;
    let mut idx = 0usize;
    while idx < lines.len() {
        if !lines[idx].starts_with("EV ") {
            idx += 1;
            continue;
        }
        let keep = lines[idx]
            .split_whitespace()
            .nth(1)
            .map(|id| used.contains(id))
            .unwrap_or(false);
        if !keep {
            lines.remove(idx);
            changed = true;
            continue;
        }
        idx += 1;
    }

    changed
}

fn prune_unused_dict_lines(lines: &mut Vec<String>) -> bool {
    let mut used: HashSet<String> = HashSet::new();
    for line in lines.iter().filter(|line| !line.starts_with("D ")) {
        collect_dict_ids(line, &mut used);
    }

    let mut changed = false;
    let mut idx = 0usize;
    while idx < lines.len() {
        if !lines[idx].starts_with("D ") {
            idx += 1;
            continue;
        }
        let keep = lines[idx]
            .split_whitespace()
            .nth(1)
            .map(|id| used.contains(id))
            .unwrap_or(false);
        if !keep {
            lines.remove(idx);
            changed = true;
            continue;
        }
        idx += 1;
    }
    changed
}

fn collect_dict_ids(line: &str, out: &mut HashSet<String>) {
    let bytes = line.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] != b'd' || idx + 1 >= bytes.len() || !bytes[idx + 1].is_ascii_digit() {
            idx += 1;
            continue;
        }
        let start = idx;
        let mut end = idx + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        out.insert(line[start..end].to_string());
        idx = end;
    }
}

pub(super) async fn extract_code_outline(root: &Path, focus_rel: &str) -> Vec<OutlineSymbol> {
    const MAX_FILE_BYTES: u64 = 512 * 1024;
    const MAX_SYMBOLS: usize = 8;

    let focus_lc = focus_rel.to_ascii_lowercase();
    if !is_code_file_candidate(&focus_lc) {
        return Vec::new();
    }

    let abs = root.join(focus_rel);
    let Ok(meta) = tokio::fs::metadata(&abs).await else {
        return Vec::new();
    };
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Vec::new();
    }

    let outline = tokio::task::spawn_blocking(move || {
        let chunker = Chunker::new(ChunkerConfig {
            // Outline is a “meaning read”: avoid pulling long docs into the metadata.
            include_documentation: false,
            ..ChunkerConfig::default()
        });
        let chunks = chunker.chunk_file(abs).ok()?;

        let mut seen: HashSet<String> = HashSet::new();
        let mut symbols: Vec<(u8, OutlineSymbol)> = Vec::new();
        for chunk in chunks {
            let Some(chunk_type) = chunk.metadata.chunk_type else {
                continue;
            };
            if !chunk_type.is_declaration() {
                continue;
            }

            let name = chunk.metadata.qualified_name.as_ref().cloned().or_else(|| {
                let symbol = chunk.metadata.symbol_name.as_ref()?.trim();
                if symbol.is_empty() {
                    return None;
                }
                if let Some(scope) = chunk.metadata.parent_scope.as_ref().map(|s| s.trim()) {
                    if !scope.is_empty() {
                        return Some(format!("{scope}.{symbol}"));
                    }
                }
                Some(symbol.to_string())
            });
            let Some(name) = name else {
                continue;
            };

            let key = format!(
                "{}:{}:{}:{}",
                chunk.file_path, chunk.start_line, chunk.end_line, name
            );
            if !seen.insert(key) {
                continue;
            }

            symbols.push((
                chunk_type.priority(),
                OutlineSymbol {
                    kind: chunk_type.as_str(),
                    name,
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    confidence: 1.0,
                },
            ));
        }

        symbols.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.start_line.cmp(&b.1.start_line))
                .then_with(|| a.1.name.cmp(&b.1.name))
        });

        let out = symbols
            .into_iter()
            .map(|(_, s)| s)
            .take(MAX_SYMBOLS)
            .collect::<Vec<_>>();
        Some(out)
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default();

    outline
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
