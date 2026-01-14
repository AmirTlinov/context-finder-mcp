use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use serde::Deserialize;
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

fn locate_repo_root() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .ancestors()
        .nth(2)
        .context("failed to resolve repo root from CARGO_MANIFEST_DIR")?;
    Ok(repo_root.to_path_buf())
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
            || line.starts_with("BROKER ")
            || line.starts_with("ANCHOR ")
            || line.starts_with("STEP ")
            || line.starts_with("AREA ");
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

#[derive(Debug, Deserialize)]
struct MeaningEvalDataset {
    schema_version: u32,
    cases: Vec<MeaningEvalCase>,
}

#[derive(Debug, Deserialize)]
struct MeaningEvalCase {
    id: String,
    fixture: String,
    query: String,
    #[serde(default)]
    expect_paths: Vec<String>,
    #[serde(default)]
    expect_claims: Vec<String>,
    #[serde(default)]
    expect_anchor_kinds: Vec<String>,
    #[serde(default)]
    forbid_map_paths: Vec<String>,
    #[serde(default)]
    min_token_saved: Option<f64>,
}

fn validate_meaning_dataset(dataset: &MeaningEvalDataset) -> Result<()> {
    anyhow::ensure!(
        dataset.schema_version == 1,
        "Unsupported meaning eval dataset schema_version {} (expected 1)",
        dataset.schema_version
    );
    anyhow::ensure!(
        !dataset.cases.is_empty(),
        "Meaning eval dataset must contain at least one case"
    );
    for case in &dataset.cases {
        anyhow::ensure!(
            !case.id.trim().is_empty(),
            "Meaning eval dataset case id must not be empty"
        );
        anyhow::ensure!(
            !case.fixture.trim().is_empty(),
            "Meaning eval dataset case '{}' fixture must not be empty",
            case.id
        );
        anyhow::ensure!(
            !case.query.trim().is_empty(),
            "Meaning eval dataset case '{}' query must not be empty",
            case.id
        );

        for kind in &case.expect_anchor_kinds {
            anyhow::ensure!(
                !kind.trim().is_empty(),
                "Meaning eval dataset case '{}' expect_anchor_kinds must not contain empty values",
                case.id
            );
        }
        for path in &case.forbid_map_paths {
            anyhow::ensure!(
                !path.trim().is_empty(),
                "Meaning eval dataset case '{}' forbid_map_paths must not contain empty values",
                case.id
            );
        }
    }
    Ok(())
}

fn build_fixture(root: &std::path::Path, fixture: &str) -> Result<()> {
    match fixture {
        "rust_contract_broker_flow" => build_fixture_rust_contract_broker_flow(root),
        "node_http_openapi" => build_fixture_node_http_openapi(root),
        "python_cli_schema" => build_fixture_python_cli_schema(root),
        "k8s_kafka_broker" => build_fixture_k8s_kafka_broker(root),
        "kustomize_kafka_broker" => build_fixture_kustomize_kafka_broker(root),
        "helm_nats_broker" => build_fixture_helm_nats_broker(root),
        "helmfile_nats_broker" => build_fixture_helmfile_nats_broker(root),
        "terraform_kafka_broker" => build_fixture_terraform_kafka_broker(root),
        "terragrunt_kafka_broker" => build_fixture_terragrunt_kafka_broker(root),
        "flux_helmrelease_nats_broker" => build_fixture_flux_helmrelease_nats_broker(root),
        "argocd_application_kafka_broker" => build_fixture_argocd_application_kafka_broker(root),
        "skaffold_kafka_broker" => build_fixture_skaffold_kafka_broker(root),
        "tiltfile_kafka_broker" => build_fixture_tiltfile_kafka_broker(root),
        "canon_howto_anchors" => build_fixture_canon_howto_anchors(root),
        "artifact_store_anti_noise" => build_fixture_artifact_store_anti_noise(root),
        "pinocchio_like_sense_map" => build_fixture_pinocchio_like_sense_map(root),
        "ci_only_canon_loop" => build_fixture_ci_only_canon_loop(root),
        "dataset_noise_budget" => build_fixture_dataset_noise_budget(root),
        "monorepo_rust_workspace" => build_fixture_monorepo_rust_workspace(root),
        "no_docs_contracts_and_ci" => build_fixture_no_docs_contracts_and_ci(root),
        "generated_noise_budget" => build_fixture_generated_noise_budget(root),
        _ => anyhow::bail!("Unknown meaning eval fixture '{fixture}'"),
    }
}

fn build_fixture_rust_contract_broker_flow(root: &std::path::Path) -> Result<()> {
    build_fixture_rust_contract_broker_flow_with_prefix(root, "user")
}

fn build_fixture_node_http_openapi(root: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::create_dir_all(root.join("contracts")).context("mkdir contracts")?;

    std::fs::write(
        root.join("package.json"),
        r#"{
  "name": "demo-node-service",
  "private": true,
  "type": "commonjs",
  "main": "src/index.js",
  "scripts": {
    "start": "node src/index.js"
  },
  "dependencies": {
    "express": "^4.18.0"
  }
}
"#,
    )
    .context("write package.json")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let mut index_lines = vec![
        "const express = require(\"express\");".to_string(),
        "const app = express();".to_string(),
        "".to_string(),
        "app.get(\"/health\", (_req, res) => res.json({ ok: true }));".to_string(),
        "".to_string(),
        "const port = process.env.PORT || 3000;".to_string(),
        "app.listen(port, () => console.log(`listening on ${port}`));".to_string(),
        "".to_string(),
    ];
    index_lines.extend((0..200).map(|idx| format!("// {idx}: {filler}")));
    index_lines.push(String::new());

    std::fs::write(root.join("src").join("index.js"), index_lines.join("\n"))
        .context("write src/index.js")?;

    let mut openapi_lines = vec![
        "openapi: 3.0.0".to_string(),
        "info:".to_string(),
        "  title: Demo Node Service".to_string(),
        "  version: 1.0.0".to_string(),
        "paths:".to_string(),
        "  /health:".to_string(),
        "    get:".to_string(),
        "      responses:".to_string(),
        "        '200':".to_string(),
        "          description: OK".to_string(),
        "".to_string(),
    ];
    openapi_lines.extend((0..180).map(|idx| format!("# {idx}: {filler}")));
    openapi_lines.push(String::new());

    std::fs::write(
        root.join("contracts").join("openapi.yaml"),
        openapi_lines.join("\n"),
    )
    .context("write contracts/openapi.yaml")?;

    Ok(())
}

fn build_fixture_python_cli_schema(root: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(root.join("contracts")).context("mkdir contracts")?;

    std::fs::write(
        root.join("pyproject.toml"),
        r#"[project]
name = "demo-python-service"
version = "0.1.0"
requires-python = ">=3.10"
dependencies = []
"#,
    )
    .context("write pyproject.toml")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let mut app_lines = vec![
        "import argparse".to_string(),
        "".to_string(),
        "def main() -> None:".to_string(),
        "    parser = argparse.ArgumentParser()".to_string(),
        "    parser.add_argument(\"--config\", default=\"config.yaml\")".to_string(),
        "    args = parser.parse_args()".to_string(),
        "    print(\"ok\", args.config)".to_string(),
        "".to_string(),
        "if __name__ == \"__main__\":".to_string(),
        "    main()".to_string(),
        "".to_string(),
    ];
    app_lines.extend((0..200).map(|idx| format!("# {idx}: {filler}")));
    app_lines.push(String::new());

    std::fs::write(root.join("app.py"), app_lines.join("\n")).context("write app.py")?;

    let schema_fill = (0..220)
        .map(|idx| format!("    \"fill_{idx}\": \"{filler}\""))
        .collect::<Vec<_>>()
        .join(",\n");
    let schema = format!(
        "{{\n  \"$schema\": \"https://json-schema.org/draft/2020-12/schema\",\n  \"title\": \"Demo Config\",\n  \"type\": \"object\",\n  \"properties\": {{\n    \"mode\": {{ \"type\": \"string\", \"description\": \"mode\" }},\n    \"port\": {{ \"type\": \"integer\", \"description\": \"port\" }},\n{schema_fill}\n  }},\n  \"required\": [\"mode\"]\n}}\n"
    );
    std::fs::write(root.join("contracts").join("example.schema.json"), schema)
        .context("write contracts/example.schema.json")?;

    Ok(())
}

fn build_fixture_k8s_kafka_broker(root: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(root.join("k8s")).context("mkdir k8s")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let mut lines = vec![
        "apiVersion: apps/v1".to_string(),
        "kind: StatefulSet".to_string(),
        "metadata:".to_string(),
        "  name: kafka".to_string(),
        "spec:".to_string(),
        "  serviceName: kafka".to_string(),
        "  selector:".to_string(),
        "    matchLabels:".to_string(),
        "      app: kafka".to_string(),
        "  template:".to_string(),
        "    metadata:".to_string(),
        "      labels:".to_string(),
        "        app: kafka".to_string(),
        "    spec:".to_string(),
        "      containers:".to_string(),
        "        - name: kafka".to_string(),
        "          image: bitnami/kafka:3.5.1".to_string(),
        "          ports:".to_string(),
        "            - containerPort: 9092".to_string(),
        "".to_string(),
    ];
    lines.extend((0..240).map(|idx| format!("# {idx}: {filler}")));
    lines.push(String::new());

    std::fs::write(root.join("k8s").join("kafka.yaml"), lines.join("\n"))
        .context("write k8s/kafka.yaml")?;
    Ok(())
}

fn build_fixture_kustomize_kafka_broker(root: &std::path::Path) -> Result<()> {
    let dir = root.join("manifests");
    std::fs::create_dir_all(&dir).context("mkdir manifests")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let mut lines = vec![
        "apiVersion: kustomize.config.k8s.io/v1beta1".to_string(),
        "kind: Kustomization".to_string(),
        "resources:".to_string(),
        "  - kafka.yaml".to_string(),
        "".to_string(),
        "# kustomize points to kafka deployment resources.".to_string(),
        "# broker: kafka".to_string(),
        "".to_string(),
    ];
    lines.extend((0..260).map(|idx| format!("# {idx}: {filler}")));
    lines.push(String::new());

    std::fs::write(dir.join("kustomization.yaml"), lines.join("\n"))
        .context("write manifests/kustomization.yaml")?;
    Ok(())
}

fn build_fixture_helm_nats_broker(root: &std::path::Path) -> Result<()> {
    let chart_dir = root.join("charts").join("myapp");
    std::fs::create_dir_all(&chart_dir).context("mkdir charts/myapp")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let mut chart_lines = vec![
        "apiVersion: v2".to_string(),
        "name: myapp".to_string(),
        "description: Demo chart".to_string(),
        "type: application".to_string(),
        "version: 0.1.0".to_string(),
        "appVersion: 1.0.0".to_string(),
        "".to_string(),
        "dependencies:".to_string(),
        "  - name: nats".to_string(),
        "    version: 1.0.0".to_string(),
        "    repository: https://nats-io.github.io/k8s/helm/charts".to_string(),
        "".to_string(),
    ];
    chart_lines.extend((0..240).map(|idx| format!("# {idx}: {filler}")));
    chart_lines.push(String::new());
    std::fs::write(chart_dir.join("Chart.yaml"), chart_lines.join("\n"))
        .context("write charts/myapp/Chart.yaml")?;

    let mut values_lines = vec![
        "replicaCount: 1".to_string(),
        "".to_string(),
        "nats:".to_string(),
        "  enabled: true".to_string(),
        "  url: nats://nats:4222".to_string(),
        "".to_string(),
    ];
    values_lines.extend((0..240).map(|idx| format!("# {idx}: {filler}")));
    values_lines.push(String::new());
    std::fs::write(chart_dir.join("values.yaml"), values_lines.join("\n"))
        .context("write charts/myapp/values.yaml")?;

    Ok(())
}

fn build_fixture_helmfile_nats_broker(root: &std::path::Path) -> Result<()> {
    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let mut lines = vec![
        "repositories:".to_string(),
        "  - name: nats".to_string(),
        "    url: https://nats-io.github.io/k8s/helm/charts".to_string(),
        "".to_string(),
        "releases:".to_string(),
        "  - name: nats".to_string(),
        "    namespace: default".to_string(),
        "    chart: nats/nats".to_string(),
        "    version: 1.0.0".to_string(),
        "".to_string(),
        "# Helmfile drives infra and depends on NATS.".to_string(),
        "# broker: nats".to_string(),
        "".to_string(),
    ];
    lines.extend((0..260).map(|idx| format!("# {idx}: {filler}")));
    lines.push(String::new());

    std::fs::write(root.join("helmfile.yaml"), lines.join("\n")).context("write helmfile.yaml")?;
    Ok(())
}

fn build_fixture_terraform_kafka_broker(root: &std::path::Path) -> Result<()> {
    let infra_dir = root.join("infra");
    std::fs::create_dir_all(&infra_dir).context("mkdir infra")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let mut lines = vec![
        "terraform {".to_string(),
        "  required_version = \">= 1.4.0\"".to_string(),
        "}".to_string(),
        "".to_string(),
        "# Infra references Kafka as a broker dependency.".to_string(),
        "variable \"kafka_brokers\" {".to_string(),
        "  type = list(string)".to_string(),
        "  default = [\"kafka:9092\"]".to_string(),
        "}".to_string(),
        "".to_string(),
        "module \"kafka\" {".to_string(),
        "  source = \"confluentinc/cp-kafka\"".to_string(),
        "}".to_string(),
        "".to_string(),
    ];
    lines.extend((0..260).map(|idx| format!("# {idx}: {filler}")));
    lines.push(String::new());

    std::fs::write(infra_dir.join("main.tf"), lines.join("\n")).context("write infra/main.tf")?;
    Ok(())
}

fn build_fixture_terragrunt_kafka_broker(root: &std::path::Path) -> Result<()> {
    let infra_dir = root.join("infra");
    std::fs::create_dir_all(&infra_dir).context("mkdir infra")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let mut lines = vec![
        "terraform {".to_string(),
        "  source = \"git::ssh://example.invalid/infra-modules.git//kafka?ref=v1.0.0\"".to_string(),
        "}".to_string(),
        "".to_string(),
        "# Terragrunt references Kafka brokers.".to_string(),
        "inputs = {".to_string(),
        "  kafka_brokers = [\"kafka:9092\"]".to_string(),
        "}".to_string(),
        "".to_string(),
    ];
    lines.extend((0..260).map(|idx| format!("# {idx}: {filler}")));
    lines.push(String::new());

    std::fs::write(infra_dir.join("terragrunt.hcl"), lines.join("\n"))
        .context("write infra/terragrunt.hcl")?;
    Ok(())
}

fn build_fixture_flux_helmrelease_nats_broker(root: &std::path::Path) -> Result<()> {
    let gitops_dir = root.join("gitops");
    std::fs::create_dir_all(&gitops_dir).context("mkdir gitops")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let mut lines = vec![
        "apiVersion: helm.toolkit.fluxcd.io/v2beta1".to_string(),
        "kind: HelmRelease".to_string(),
        "metadata:".to_string(),
        "  name: nats".to_string(),
        "spec:".to_string(),
        "  interval: 5m".to_string(),
        "  chart:".to_string(),
        "    spec:".to_string(),
        "      chart: nats".to_string(),
        "      sourceRef:".to_string(),
        "        kind: HelmRepository".to_string(),
        "        name: nats".to_string(),
        "  values:".to_string(),
        "    nats:".to_string(),
        "      url: nats://nats:4222".to_string(),
        "".to_string(),
    ];
    lines.extend((0..260).map(|idx| format!("# {idx}: {filler}")));
    lines.push(String::new());

    std::fs::write(gitops_dir.join("helmrelease.yaml"), lines.join("\n"))
        .context("write gitops/helmrelease.yaml")?;
    Ok(())
}

fn build_fixture_argocd_application_kafka_broker(root: &std::path::Path) -> Result<()> {
    let argocd_dir = root.join("argocd");
    std::fs::create_dir_all(&argocd_dir).context("mkdir argocd")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let mut lines = vec![
        "apiVersion: argoproj.io/v1alpha1".to_string(),
        "kind: Application".to_string(),
        "metadata:".to_string(),
        "  name: kafka".to_string(),
        "spec:".to_string(),
        "  project: default".to_string(),
        "  source:".to_string(),
        "    repoURL: https://charts.bitnami.com/bitnami".to_string(),
        "    chart: kafka".to_string(),
        "    targetRevision: 26.7.2".to_string(),
        "    helm:".to_string(),
        "      values: |".to_string(),
        "        listeners:".to_string(),
        "          client:".to_string(),
        "            protocol: PLAINTEXT".to_string(),
        "  destination:".to_string(),
        "    server: https://kubernetes.default.svc".to_string(),
        "    namespace: kafka".to_string(),
        "".to_string(),
    ];
    lines.extend((0..260).map(|idx| format!("# {idx}: {filler}")));
    lines.push(String::new());

    std::fs::write(argocd_dir.join("application.yaml"), lines.join("\n"))
        .context("write argocd/application.yaml")?;
    Ok(())
}

fn build_fixture_skaffold_kafka_broker(root: &std::path::Path) -> Result<()> {
    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let mut lines = vec![
        "apiVersion: skaffold/v4beta6".to_string(),
        "kind: Config".to_string(),
        "metadata:".to_string(),
        "  name: demo".to_string(),
        "build:".to_string(),
        "  artifacts:".to_string(),
        "    - image: bitnami/kafka".to_string(),
        "      docker:".to_string(),
        "        dockerfile: Dockerfile".to_string(),
        "deploy:".to_string(),
        "  kubectl:".to_string(),
        "    manifests:".to_string(),
        "      - k8s/*.yaml".to_string(),
        "".to_string(),
    ];
    lines.extend((0..260).map(|idx| format!("# {idx}: {filler}")));
    lines.push(String::new());

    std::fs::write(root.join("skaffold.yaml"), lines.join("\n")).context("write skaffold.yaml")?;
    Ok(())
}

fn build_fixture_tiltfile_kafka_broker(root: &std::path::Path) -> Result<()> {
    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let mut lines = vec![
        "# Tiltfile that references Kafka for local dev.".to_string(),
        "docker_build('bitnami/kafka', '.')".to_string(),
        "k8s_yaml('k8s/kafka.yaml')".to_string(),
        "k8s_resource('kafka')".to_string(),
        "".to_string(),
    ];
    lines.extend((0..260).map(|idx| format!("# {idx}: {filler}")));
    lines.push(String::new());

    std::fs::write(root.join("Tiltfile"), lines.join("\n")).context("write Tiltfile")?;
    Ok(())
}

fn build_fixture_canon_howto_anchors(root: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";

    let mut readme = vec![
        "# Demo Repo".to_string(),
        "".to_string(),
        "## Quick Start".to_string(),
        "Run tests and sanity checks via the Makefile.".to_string(),
        "".to_string(),
    ];
    readme.extend((0..260).map(|idx| format!("{idx}: {filler}")));
    readme.push(String::new());
    std::fs::write(root.join("README.md"), readme.join("\n")).context("write README.md")?;

    let mut makefile = vec![
        ".PHONY: test run lint fmt".to_string(),
        "".to_string(),
        "test:".to_string(),
        "\t@echo running tests".to_string(),
        "".to_string(),
        "run:".to_string(),
        "\t@echo running app".to_string(),
        "".to_string(),
        "lint:".to_string(),
        "\t@echo lint".to_string(),
        "".to_string(),
        "fmt:".to_string(),
        "\t@echo fmt".to_string(),
        "".to_string(),
    ];
    makefile.extend((0..260).map(|idx| format!("# {idx}: {filler}")));
    makefile.push(String::new());
    std::fs::write(root.join("Makefile"), makefile.join("\n")).context("write Makefile")?;

    let main_body = std::iter::once("fn main() { println!(\"ok\"); }".to_string())
        .chain((0..200).map(|idx| format!("// {idx}: {filler}")))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(root.join("src").join("main.rs"), main_body).context("write src/main.rs")?;

    Ok(())
}

fn build_fixture_artifact_store_anti_noise(root: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::create_dir_all(root.join("artifacts").join("runs").join("run1"))
        .context("mkdir artifacts/runs/run1")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";

    let mut artifacts_readme = vec![
        "# Artifacts".to_string(),
        "".to_string(),
        "## Layout".to_string(),
        "Outputs live under `artifacts/runs/<run_id>/...`.".to_string(),
        "".to_string(),
        "## Naming".to_string(),
        "Each run emits JSON files with stable prefixes.".to_string(),
        "".to_string(),
    ];
    artifacts_readme.extend((0..160).map(|idx| format!("{idx}: {filler}")));
    artifacts_readme.push(String::new());
    std::fs::write(
        root.join("artifacts").join("README.md"),
        artifacts_readme.join("\n"),
    )
    .context("write artifacts/README.md")?;

    // Simulate an artifact-heavy repo: many JSON outputs under a single run directory.
    // This should never dominate `S MAP`; it should be represented as an evidence-backed anchor.
    let run_dir = root.join("artifacts").join("runs").join("run1");
    for idx in 0..320usize {
        let payload = format!("{{\"run\":\"run1\",\"idx\":{idx},\"note\":\"{filler}\"}}\n");
        let name = format!("result_{idx:04}.json");
        std::fs::write(run_dir.join(name), payload)
            .with_context(|| format!("write artifacts/runs/run1/result_{idx:04}.json"))?;
    }

    let main_body = std::iter::once("fn main() { println!(\"ok\"); }".to_string())
        .chain((0..200).map(|idx| format!("// {idx}: {filler}")))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(root.join("src").join("main.rs"), main_body).context("write src/main.rs")?;

    Ok(())
}

fn build_fixture_pinocchio_like_sense_map(root: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(root.join("src").join("runtime")).context("mkdir src/runtime")?;
    std::fs::create_dir_all(root.join("docs").join("contracts")).context("mkdir docs/contracts")?;
    std::fs::create_dir_all(root.join("baselines")).context("mkdir baselines")?;
    std::fs::create_dir_all(root.join("artifacts").join("runs").join("run1"))
        .context("mkdir artifacts/runs/run1")?;
    std::fs::create_dir_all(root.join("k8s")).context("mkdir k8s")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";

    let readme = vec![
        "# Pinocchio-like Research Repo".to_string(),
        "".to_string(),
        "This repo is artifact-heavy and experiment-driven.".to_string(),
        "".to_string(),
        "## Canon".to_string(),
        "Start with baselines and contracts, then run the Makefile targets.".to_string(),
        "".to_string(),
    ]
    .into_iter()
    .chain((0..120).map(|idx| format!("{idx}: {filler}")))
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    std::fs::write(root.join("README.md"), readme).context("write README.md")?;

    let makefile = vec![
        ".PHONY: setup test run eval lint fmt".to_string(),
        "".to_string(),
        "setup:".to_string(),
        "\t@echo setup".to_string(),
        "".to_string(),
        "test:".to_string(),
        "\t@echo test".to_string(),
        "".to_string(),
        "run:".to_string(),
        "\t@echo run".to_string(),
        "".to_string(),
        "eval:".to_string(),
        "\t@echo eval".to_string(),
        "".to_string(),
        "lint:".to_string(),
        "\t@echo lint".to_string(),
        "".to_string(),
        "fmt:".to_string(),
        "\t@echo fmt".to_string(),
        "".to_string(),
        format!("# {filler}"),
    ]
    .join("\n")
        + "\n";
    std::fs::write(root.join("Makefile"), makefile).context("write Makefile")?;

    let protocol = vec![
        "# Protocol".to_string(),
        "".to_string(),
        "This is an artifact contract for produced outputs.".to_string(),
        "".to_string(),
        "## Fields".to_string(),
        "- run_id".to_string(),
        "- metrics".to_string(),
        "".to_string(),
    ]
    .into_iter()
    .chain((0..80).map(|idx| format!("{idx}: {filler}")))
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    std::fs::write(
        root.join("docs").join("contracts").join("protocol.md"),
        protocol,
    )
    .context("write docs/contracts/protocol.md")?;

    let baselines = vec![
        "# Baselines".to_string(),
        "".to_string(),
        "## Evaluation".to_string(),
        "Run the evaluation suite via `make eval`.".to_string(),
        "".to_string(),
    ]
    .into_iter()
    .chain((0..120).map(|idx| format!("{idx}: {filler}")))
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    std::fs::write(root.join("baselines").join("README.md"), baselines)
        .context("write baselines/README.md")?;

    let artifacts_readme = vec![
        "# Artifacts".to_string(),
        "".to_string(),
        "## Layout".to_string(),
        "Outputs live under `artifacts/runs/<run_id>/...`.".to_string(),
        "".to_string(),
    ]
    .into_iter()
    .chain((0..80).map(|idx| format!("{idx}: {filler}")))
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    std::fs::write(root.join("artifacts").join("README.md"), artifacts_readme)
        .context("write artifacts/README.md")?;

    // Simulate an artifact-heavy run directory: lots of JSON outputs.
    let run_dir = root.join("artifacts").join("runs").join("run1");
    for idx in 0..160usize {
        let payload = format!("{{\"run\":\"run1\",\"idx\":{idx},\"note\":\"{filler}\"}}\n");
        let name = format!("result_{idx:04}.json");
        std::fs::write(run_dir.join(name), payload)
            .with_context(|| format!("write artifacts/runs/run1/result_{idx:04}.json"))?;
    }

    let runtime = [
        "pub struct Runtime {}".to_string(),
        "impl Runtime {".to_string(),
        "  pub fn run(&self) {}".to_string(),
        "}".to_string(),
        format!("// {filler}"),
    ]
    .join("\n")
        + "\n";
    std::fs::write(root.join("src").join("runtime").join("mod.rs"), runtime)
        .context("write src/runtime/mod.rs")?;

    let k8s = [
        "apiVersion: apps/v1".to_string(),
        "kind: Deployment".to_string(),
        "metadata:".to_string(),
        "  name: demo".to_string(),
        "spec: {}".to_string(),
        format!("# {filler}"),
    ]
    .join("\n")
        + "\n";
    std::fs::write(root.join("k8s").join("app.yaml"), k8s).context("write k8s/app.yaml")?;

    Ok(())
}

fn build_fixture_ci_only_canon_loop(root: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(root.join(".github").join("workflows"))
        .context("mkdir .github/workflows")?;
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;

    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "demo-ci-canon"
version = "0.1.0"
edition = "2021"

[dependencies]
"#,
    )
    .context("write Cargo.toml")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let main_body = std::iter::once("fn main() { println!(\"ok\"); }".to_string())
        .chain((0..240).map(|idx| format!("// {idx}: {filler}")))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(root.join("src").join("main.rs"), main_body).context("write src/main.rs")?;

    // CI is the only reliable "canon" here: no README, no Makefile.
    // Keep the workflow verbose so evidence windows have a big baseline.
    let mut workflow_lines = vec![
        "name: CI".to_string(),
        "on:".to_string(),
        "  push:".to_string(),
        "  pull_request:".to_string(),
        "".to_string(),
        "jobs:".to_string(),
        "  gates:".to_string(),
        "    runs-on: ubuntu-latest".to_string(),
        "    steps:".to_string(),
        "      - uses: actions/checkout@v4".to_string(),
        "      - name: fmt".to_string(),
        "        run: cargo fmt --all -- --check".to_string(),
        "      - name: clippy".to_string(),
        "        run: cargo clippy --workspace --all-targets -- -D warnings".to_string(),
        "      - name: test".to_string(),
        "        run: cargo test --workspace".to_string(),
        "".to_string(),
    ];
    workflow_lines.extend((0..220).map(|idx| format!("# {idx}: {filler}")));
    workflow_lines.push(String::new());
    std::fs::write(
        root.join(".github").join("workflows").join("ci.yml"),
        workflow_lines.join("\n"),
    )
    .context("write .github/workflows/ci.yml")?;

    Ok(())
}

fn build_fixture_dataset_noise_budget(root: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::create_dir_all(root.join("data")).context("mkdir data")?;

    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "demo-dataset-heavy"
version = "0.1.0"
edition = "2021"

[dependencies]
"#,
    )
    .context("write Cargo.toml")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let main_body = std::iter::once("fn main() { println!(\"ok\"); }".to_string())
        .chain((0..240).map(|idx| format!("// {idx}: {filler}")))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(root.join("src").join("main.rs"), main_body).context("write src/main.rs")?;

    // Dataset-heavy repos should not drown MAP: only code/CI/contracts count by default.
    let row = format!("col_a,col_b\n1,{filler}\n");
    for idx in 0..220usize {
        let name = format!("part_{idx:04}.csv");
        std::fs::write(root.join("data").join(name), &row)
            .with_context(|| format!("write data/part_{idx:04}.csv"))?;
    }

    Ok(())
}

fn build_fixture_monorepo_rust_workspace(root: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(root.join(".github").join("workflows"))
        .context("mkdir .github/workflows")?;
    std::fs::create_dir_all(root.join("crates").join("app").join("src"))
        .context("mkdir crates/app/src")?;
    std::fs::create_dir_all(root.join("crates").join("lib").join("src"))
        .context("mkdir crates/lib/src")?;

    std::fs::write(
        root.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/app", "crates/lib"]
resolver = "2"
"#,
    )
    .context("write Cargo.toml")?;

    std::fs::write(
        root.join("crates").join("lib").join("Cargo.toml"),
        r#"[package]
name = "demo-lib"
version = "0.1.0"
edition = "2021"

[dependencies]
"#,
    )
    .context("write crates/lib/Cargo.toml")?;

    std::fs::write(
        root.join("crates").join("app").join("Cargo.toml"),
        r#"[package]
name = "demo-app"
version = "0.1.0"
edition = "2021"

[dependencies]
demo-lib = { path = "../lib" }
"#,
    )
    .context("write crates/app/Cargo.toml")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let lib_body = std::iter::once("pub fn answer() -> u32 { 42 }".to_string())
        .chain((0..220).map(|idx| format!("// {idx}: {filler}")))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(
        root.join("crates").join("lib").join("src").join("lib.rs"),
        lib_body,
    )
    .context("write crates/lib/src/lib.rs")?;

    let app_body =
        std::iter::once("fn main() { println!(\"{}\", demo_lib::answer()); }".to_string())
            .chain((0..220).map(|idx| format!("// {idx}: {filler}")))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
    std::fs::write(
        root.join("crates").join("app").join("src").join("main.rs"),
        app_body,
    )
    .context("write crates/app/src/main.rs")?;

    let mut readme_lines = vec![
        "# Monorepo Rust Workspace".to_string(),
        "".to_string(),
        "## Canon".to_string(),
        "Run CI-equivalent checks locally via: `cargo test --workspace`.".to_string(),
        "".to_string(),
    ];
    readme_lines.extend((0..180).map(|idx| format!("{idx}: {filler}")));
    readme_lines.push(String::new());
    std::fs::write(root.join("README.md"), readme_lines.join("\n")).context("write README.md")?;

    let mut workflow_lines = vec![
        "name: CI".to_string(),
        "on: [push, pull_request]".to_string(),
        "".to_string(),
        "jobs:".to_string(),
        "  gates:".to_string(),
        "    runs-on: ubuntu-latest".to_string(),
        "    steps:".to_string(),
        "      - uses: actions/checkout@v4".to_string(),
        "      - name: test".to_string(),
        "        run: cargo test --workspace".to_string(),
        "      - name: fmt".to_string(),
        "        run: cargo fmt --all -- --check".to_string(),
        "      - name: clippy".to_string(),
        "        run: cargo clippy --workspace --all-targets -- -D warnings".to_string(),
        "".to_string(),
    ];
    workflow_lines.extend((0..200).map(|idx| format!("# {idx}: {filler}")));
    workflow_lines.push(String::new());
    std::fs::write(
        root.join(".github").join("workflows").join("ci.yml"),
        workflow_lines.join("\n"),
    )
    .context("write .github/workflows/ci.yml")?;

    Ok(())
}

fn build_fixture_no_docs_contracts_and_ci(root: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(root.join(".github").join("workflows"))
        .context("mkdir .github/workflows")?;
    std::fs::create_dir_all(root.join("contracts")).context("mkdir contracts")?;
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;

    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "demo-no-docs"
version = "0.1.0"
edition = "2021"

[dependencies]
"#,
    )
    .context("write Cargo.toml")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let main_body = std::iter::once("fn main() { println!(\"ok\"); }".to_string())
        .chain((0..220).map(|idx| format!("// {idx}: {filler}")))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(root.join("src").join("main.rs"), main_body).context("write src/main.rs")?;

    let mut openapi_lines = vec![
        "openapi: 3.0.0".to_string(),
        "info:".to_string(),
        "  title: Demo API".to_string(),
        "  version: 0.1.0".to_string(),
        "paths:".to_string(),
        "  /health:".to_string(),
        "    get:".to_string(),
        "      responses:".to_string(),
        "        '200':".to_string(),
        "          description: ok".to_string(),
        "".to_string(),
    ];
    openapi_lines.extend((0..220).map(|idx| format!("# {idx}: {filler}")));
    openapi_lines.push(String::new());
    std::fs::write(
        root.join("contracts").join("openapi.yaml"),
        openapi_lines.join("\n"),
    )
    .context("write contracts/openapi.yaml")?;

    let mut workflow_lines = vec![
        "name: CI".to_string(),
        "on: [push, pull_request]".to_string(),
        "".to_string(),
        "jobs:".to_string(),
        "  gates:".to_string(),
        "    runs-on: ubuntu-latest".to_string(),
        "    steps:".to_string(),
        "      - uses: actions/checkout@v4".to_string(),
        "      - name: test".to_string(),
        "        run: cargo test --workspace".to_string(),
        "".to_string(),
    ];
    workflow_lines.extend((0..220).map(|idx| format!("# {idx}: {filler}")));
    workflow_lines.push(String::new());
    std::fs::write(
        root.join(".github").join("workflows").join("ci.yml"),
        workflow_lines.join("\n"),
    )
    .context("write .github/workflows/ci.yml")?;

    Ok(())
}

fn build_fixture_generated_noise_budget(root: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::create_dir_all(root.join("dist")).context("mkdir dist")?;
    std::fs::create_dir_all(root.join("build")).context("mkdir build")?;
    std::fs::create_dir_all(root.join("out")).context("mkdir out")?;

    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "demo-generated-noise"
version = "0.1.0"
edition = "2021"

[dependencies]
"#,
    )
    .context("write Cargo.toml")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let main_body = std::iter::once("fn main() { println!(\"ok\"); }".to_string())
        .chain((0..220).map(|idx| format!("// {idx}: {filler}")))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(root.join("src").join("main.rs"), main_body).context("write src/main.rs")?;

    let js_line = format!("// generated: {filler}\n");
    for idx in 0..220usize {
        let name = format!("bundle_{idx:04}.js");
        std::fs::write(root.join("dist").join(name), &js_line)
            .with_context(|| format!("write dist/bundle_{idx:04}.js"))?;
    }
    let json_line = format!("{{\"generated\":true,\"note\":\"{filler}\"}}\n");
    for idx in 0..160usize {
        let name = format!("gen_{idx:04}.json");
        std::fs::write(root.join("build").join(name), &json_line)
            .with_context(|| format!("write build/gen_{idx:04}.json"))?;
    }
    let out_line = format!("{filler}\n");
    for idx in 0..160usize {
        let name = format!("out_{idx:04}.txt");
        std::fs::write(root.join("out").join(name), &out_line)
            .with_context(|| format!("write out/out_{idx:04}.txt"))?;
    }

    Ok(())
}

fn build_fixture_rust_contract_broker_flow_with_prefix(
    root: &std::path::Path,
    channel_prefix: &str,
) -> Result<()> {
    std::fs::create_dir_all(root.join("src")).context("mkdir src")?;
    std::fs::create_dir_all(root.join("contracts")).context("mkdir contracts")?;

    let filler = "filler filler filler filler filler filler filler filler filler filler filler";
    let main_body = std::iter::once("fn main() { println!(\"ok\"); }".to_string())
        .chain((0..200).map(|idx| format!("// {idx}: {filler}")))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(root.join("src").join("main.rs"), main_body).context("write src/main.rs")?;

    let schema_fill = (0..160)
        .map(|idx| format!("    \"fill_{idx}\": \"{filler}\""))
        .collect::<Vec<_>>()
        .join(",\n");
    let schema = format!(
        "{{\n  \"$schema\": \"https://json-schema.org/draft/2020-12/schema\",\n  \"type\": \"object\",\n  \"properties\": {{\n{schema_fill}\n  }}\n}}\n"
    );
    std::fs::write(root.join("contracts").join("example.schema.json"), schema)
        .context("write contracts/example.schema.json")?;

    let asyncapi = std::iter::once("asyncapi: 2.6.0".to_string())
        .chain(std::iter::once("info:".to_string()))
        .chain(std::iter::once("  title: Example".to_string()))
        .chain(std::iter::once("  version: 1.0.0".to_string()))
        .chain(std::iter::once("servers:".to_string()))
        .chain(std::iter::once("  local:".to_string()))
        .chain(std::iter::once("    url: localhost:9092".to_string()))
        .chain(std::iter::once("    protocol: kafka".to_string()))
        .chain(std::iter::once("channels:".to_string()))
        .chain(std::iter::once(format!("  {channel_prefix}.created:")))
        .chain(std::iter::once("    publish:".to_string()))
        .chain(std::iter::once("      message:".to_string()))
        .chain(std::iter::once("        name: UserCreated".to_string()))
        .chain(std::iter::once(format!("  {channel_prefix}.deleted:")))
        .chain(std::iter::once("    subscribe:".to_string()))
        .chain(std::iter::once("      message:".to_string()))
        .chain(std::iter::once("        name: UserDeleted".to_string()))
        .chain((0..160).map(|idx| format!("# {idx}: {filler}")))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(
        root.join("contracts").join("events.asyncapi.yaml"),
        asyncapi,
    )
    .context("write contracts/events.asyncapi.yaml")?;

    let compose = std::iter::once("version: '3.8'".to_string())
        .chain(std::iter::once("services:".to_string()))
        .chain(std::iter::once("  kafka:".to_string()))
        .chain(std::iter::once(
            "    image: bitnami/kafka:latest".to_string(),
        ))
        .chain(std::iter::once("    environment:".to_string()))
        .chain(std::iter::once("      - KAFKA_CFG_NODE_ID=0".to_string()))
        .chain(std::iter::once(
            "      - KAFKA_CFG_PROCESS_ROLES=broker,controller".to_string(),
        ))
        .chain(std::iter::once(
            "      - KAFKA_CFG_LISTENERS=PLAINTEXT://:9092".to_string(),
        ))
        .chain((0..200).map(|idx| format!("# {idx}: {filler}")))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(root.join("docker-compose.yml"), compose).context("write docker-compose.yml")?;

    Ok(())
}

fn parse_cp_dict(pack: &str) -> Result<std::collections::HashMap<String, String>> {
    let mut out: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in pack.lines() {
        if !line.starts_with("D ") {
            continue;
        }
        let mut parts = line.splitn(3, ' ');
        parts.next(); // "D"
        let Some(id) = parts.next() else {
            continue;
        };
        let Some(rest) = parts.next() else {
            continue;
        };
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
    root: &std::path::Path,
    rel: &str,
    start_line: usize,
    end_line: usize,
) -> Result<usize> {
    let content = std::fs::read_to_string(root.join(rel))
        .with_context(|| format!("read evidence file {rel}"))?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Ok(0);
    }
    let start = start_line
        .saturating_sub(1)
        .min(lines.len().saturating_sub(1));
    let end = end_line.min(lines.len());
    if end <= start {
        return Ok(0);
    }
    let mut total = 0usize;
    for line in &lines[start..end] {
        total = total.saturating_add(line.chars().count());
        total = total.saturating_add(1); // newline
    }
    Ok(total)
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
async fn meaning_pack_emits_next_actions_in_full_mode_only() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    std::fs::create_dir_all(root.path().join("src")).context("mkdir src")?;
    std::fs::write(
        root.path().join("src").join("main.rs"),
        "fn main() { println!(\"ok\"); }\n",
    )
    .context("write src/main.rs")?;
    std::fs::write(
        root.path().join("README.md"),
        "# Demo\n\n## Canon\nRun: `cargo test`.\n",
    )
    .context("write README.md")?;

    let service = start_mcp_server().await?;

    let resp_facts = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "query": "orient on canon and entrypoints",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        resp_facts.is_error,
        Some(true),
        "expected meaning_pack to succeed (response_mode=facts)"
    );
    let facts_text = resp_facts
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_pack facts missing text output")?;
    assert!(
        !facts_text.contains("next_actions:"),
        "expected next_actions section to be omitted in response_mode=facts"
    );

    let resp_full = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "query": "orient on canon and entrypoints",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "full",
        }),
    )
    .await?;
    assert_ne!(
        resp_full.is_error,
        Some(true),
        "expected meaning_pack to succeed (response_mode=full)"
    );
    let full_text = resp_full
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_pack full missing text output")?;
    assert!(
        full_text.contains("next_actions:"),
        "expected next_actions section to be present in response_mode=full"
    );
    assert!(
        full_text.contains("next_action tool=evidence_fetch"),
        "expected evidence_fetch next_action in response_mode=full"
    );
    assert!(
        full_text.contains("next_action tool=meaning_focus"),
        "expected meaning_focus next_action in response_mode=full"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_focus_emits_next_actions_in_full_mode_only() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    std::fs::create_dir_all(root.path().join("src")).context("mkdir src")?;
    std::fs::write(
        root.path().join("src").join("main.rs"),
        "fn main() { println!(\"ok\"); }\n",
    )
    .context("write src/main.rs")?;
    std::fs::write(
        root.path().join("README.md"),
        "# Demo\n\n## Canon\nRun: `cargo test`.\n",
    )
    .context("write README.md")?;

    let service = start_mcp_server().await?;

    let resp_facts = call_tool(
        &service,
        "meaning_focus",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "focus": "src/main.rs",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(
        resp_facts.is_error,
        Some(true),
        "expected meaning_focus to succeed (response_mode=facts)"
    );
    let facts_text = resp_facts
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_focus facts missing text output")?;
    assert!(
        !facts_text.contains("next_actions:"),
        "expected next_actions section to be omitted in response_mode=facts"
    );

    let resp_full = call_tool(
        &service,
        "meaning_focus",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "focus": "src/main.rs",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "full",
        }),
    )
    .await?;
    assert_ne!(
        resp_full.is_error,
        Some(true),
        "expected meaning_focus to succeed (response_mode=full)"
    );
    let full_text = resp_full
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_focus full missing text output")?;
    assert!(
        full_text.contains("next_actions:"),
        "expected next_actions section to be present in response_mode=full"
    );
    assert!(
        full_text.contains("next_action tool=evidence_fetch"),
        "expected evidence_fetch next_action in response_mode=full"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_pack_can_return_svg_diagram_when_requested() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    build_fixture(root.path(), "canon_howto_anchors")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "query": "orient on canon/howto and next steps",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "facts",
            "output_format": "context_and_diagram",
        }),
    )
    .await?;
    assert_ne!(resp.is_error, Some(true), "meaning_pack returned error");

    let svg_image = resp
        .content
        .iter()
        .find_map(|c| c.as_image())
        .context("missing image content in meaning_pack response")?;
    assert_eq!(
        svg_image.mime_type.as_str(),
        "image/svg+xml",
        "unexpected diagram mime type"
    );
    let svg_bytes = STANDARD
        .decode(svg_image.data.as_bytes())
        .context("decode svg base64")?;
    let svg = String::from_utf8(svg_bytes).context("svg must be utf-8")?;
    anyhow::ensure!(
        svg.contains("<svg"),
        "expected diagram payload to contain <svg"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_focus_can_return_svg_diagram_when_requested() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    build_fixture(root.path(), "canon_howto_anchors")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_focus",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "focus": "README.md",
            "query": "zoom on canon doc",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "facts",
            "output_format": "context_and_diagram",
        }),
    )
    .await?;
    assert_ne!(resp.is_error, Some(true), "meaning_focus returned error");

    let svg_image = resp
        .content
        .iter()
        .find_map(|c| c.as_image())
        .context("missing image content in meaning_focus response")?;
    assert_eq!(
        svg_image.mime_type.as_str(),
        "image/svg+xml",
        "unexpected diagram mime type"
    );
    let svg_bytes = STANDARD
        .decode(svg_image.data.as_bytes())
        .context("decode svg base64")?;
    let svg = String::from_utf8(svg_bytes).context("svg must be utf-8")?;
    anyhow::ensure!(
        svg.contains("<svg"),
        "expected diagram payload to contain <svg"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_pack_suppresses_artifact_dirs_in_map_but_emits_artifact_anchor() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    build_fixture(root.path(), "artifact_store_anti_noise")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "query": "orient on artifacts without drowning map",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(resp.is_error, Some(true), "meaning_pack returned error");

    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_pack missing text output")?;
    let pack = extract_cp_pack(text)?;
    assert_meaning_invariants(&pack)?;

    let dict = parse_cp_dict(&pack)?;

    let artifact_anchor = pack
        .lines()
        .find(|line| line.starts_with("ANCHOR ") && line.contains("kind=artifact"))
        .context("expected meaning_pack to emit an artifact anchor")?;
    let file_id = artifact_anchor
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("file="))
        .context("artifact anchor missing file=")?;
    let file = dict
        .get(file_id)
        .map(String::as_str)
        .unwrap_or_default()
        .to_string();
    assert!(
        file.contains("artifacts/README.md"),
        "expected artifact anchor file to reference artifacts/README.md (got: {file})"
    );

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
            .unwrap_or("");
        let path = dict.get(path_id).cloned().unwrap_or_default();
        assert_ne!(
            path, "artifacts",
            "expected artifacts dir to be suppressed in S MAP (anti-noise)"
        );
    }

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_pack_emits_ci_anchor_and_steps_reference_ci_evidence() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    build_fixture(root.path(), "ci_only_canon_loop")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "query": "derive canon loop from CI config",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(resp.is_error, Some(true), "meaning_pack returned error");

    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_pack missing text output")?;
    let pack = extract_cp_pack(text)?;
    assert_meaning_invariants(&pack)?;

    let dict = parse_cp_dict(&pack)?;

    let ci_anchor = pack
        .lines()
        .find(|line| line.starts_with("ANCHOR ") && line.contains("kind=ci"))
        .context("expected meaning_pack to emit a CI anchor")?;
    let ci_file_id = ci_anchor
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("file="))
        .context("CI anchor missing file=")?;
    let ci_file = dict.get(ci_file_id).cloned().unwrap_or_default();
    assert!(
        ci_file.contains(".github/workflows/ci.yml"),
        "expected CI anchor file to reference .github/workflows/ci.yml (got: {ci_file})"
    );

    let mut ev_for_ci: HashSet<&str> = HashSet::new();
    for line in pack.lines() {
        if !line.starts_with("EV ") {
            continue;
        }
        let Some(ev_id) = line
            .strip_prefix("EV ")
            .and_then(|rest| rest.split_whitespace().next())
        else {
            continue;
        };
        let Some((file_id, _, _)) = parse_ev_file_and_range(line) else {
            continue;
        };
        if file_id == ci_file_id {
            ev_for_ci.insert(ev_id);
        }
    }
    anyhow::ensure!(
        !ev_for_ci.is_empty(),
        "expected at least one EV line pointing to CI file"
    );

    let has_step_with_ci_evidence = pack.lines().any(|line| {
        if !line.starts_with("STEP ") {
            return false;
        }
        let Some(ev) = extract_ev_ref(line) else {
            return false;
        };
        ev_for_ci.contains(ev)
    });
    assert!(
        has_step_with_ci_evidence,
        "expected at least one STEP claim to reference CI evidence"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_pack_suppresses_dataset_dirs_in_map() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    build_fixture(root.path(), "dataset_noise_budget")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "query": "orient on code without drowning in datasets",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(resp.is_error, Some(true), "meaning_pack returned error");

    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_pack missing text output")?;
    let pack = extract_cp_pack(text)?;
    assert_meaning_invariants(&pack)?;

    let dict = parse_cp_dict(&pack)?;

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
            .unwrap_or("");
        let path = dict.get(path_id).cloned().unwrap_or_default();
        assert_ne!(
            path, "data",
            "expected dataset dir to be suppressed in S MAP (noise budget)"
        );
    }

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn evidence_fetch_refuses_compose_secret_assignments() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    std::fs::write(
        root.path().join("docker-compose.yml"),
        "services:\n  db:\n    environment:\n      POSTGRES_PASSWORD: supersecret\n",
    )
    .context("write docker-compose.yml")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "evidence_fetch",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "items": [{
                "file": "docker-compose.yml",
                "start_line": 1,
                "end_line": 10
            }],
            "max_chars": 2000,
            "max_lines": 50,
            "strict_hash": false,
            "response_mode": "facts"
        }),
    )
    .await?;
    assert_eq!(
        resp.is_error,
        Some(true),
        "expected evidence_fetch to refuse compose secret"
    );

    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or("");
    assert!(
        text.to_ascii_lowercase().contains("potential secret"),
        "expected error to mention potential secret"
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
async fn meaning_pack_truncation_preserves_claim_evidence_invariants() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    std::fs::create_dir_all(root.path().join("contracts")).context("mkdir contracts")?;

    // Create enough distinct evidence-backed files to force truncation at the minimum budget.
    for idx in 0..6 {
        let bin_dir = root.path().join(format!("bin{idx}")).join("src");
        std::fs::create_dir_all(&bin_dir).context("mkdir bin src")?;
        std::fs::write(bin_dir.join("main.rs"), "fn main() {}\\n").context("write main.rs")?;
    }
    for idx in 0..6 {
        std::fs::write(
            root.path()
                .join("contracts")
                .join(format!("c{idx}.schema.json")),
            "{ \"type\": \"object\" }\\n",
        )
        .context("write schema contract")?;
    }

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "query": "force truncation and keep invariants",
            "max_chars": 800,
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
async fn meaning_pack_keeps_multiple_anchors_under_tight_budgets_for_research_repos() -> Result<()>
{
    let root = tempfile::tempdir().context("temp project dir")?;
    build_fixture(root.path(), "pinocchio_like_sense_map")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "query": "orient on canon, how-to-run, contracts, experiments, artifacts",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(resp.is_error, Some(true), "meaning_pack returned error");

    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_pack missing text output")?;
    let pack = extract_cp_pack(text)?;
    assert_meaning_invariants(&pack)?;

    let anchor_count = pack
        .lines()
        .filter(|line| line.starts_with("ANCHOR "))
        .count();
    assert!(
        anchor_count >= 3,
        "expected >=3 ANCHOR lines under max_chars=2000 (got {anchor_count})"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_pack_recognizes_russian_entrypoint_intent_under_tight_budget() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    build_fixture(root.path(), "rust_contract_broker_flow")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "query": "где точки входа и как запускается проект?",
            "max_chars": 2000,
            "map_limit": 6,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(resp.is_error, Some(true), "meaning_pack returned error");

    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_pack missing text output")?;
    let pack = extract_cp_pack(text)?;
    assert_meaning_invariants(&pack)?;

    anyhow::ensure!(
        pack.contains("S ENTRYPOINTS"),
        "expected S ENTRYPOINTS section for RU entrypoint query under tight budget"
    );
    anyhow::ensure!(
        pack.lines().any(|line| line.starts_with("ENTRY ")),
        "expected at least one ENTRY claim for RU entrypoint query"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_pack_detects_c_entrypoint_from_russian_query() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    std::fs::create_dir_all(root.path().join("src")).context("mkdir src")?;
    std::fs::write(
        root.path().join("src").join("main.c"),
        "static void helper(void) {}\nint main(void) { helper(); return 0; }\n",
    )
    .context("write src/main.c")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "query": "где точки входа?",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(resp.is_error, Some(true), "meaning_pack returned error");

    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_pack missing text output")?;
    let pack = extract_cp_pack(text)?;
    assert_meaning_invariants(&pack)?;

    anyhow::ensure!(
        pack.contains("S ENTRYPOINTS"),
        "expected S ENTRYPOINTS section for C entrypoint under RU query"
    );
    anyhow::ensure!(
        pack.contains("src/main.c"),
        "expected src/main.c to appear in meaning_pack output"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_focus_emits_outline_for_c_file() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    std::fs::create_dir_all(root.path().join("src")).context("mkdir src")?;
    std::fs::write(
        root.path().join("src").join("main.c"),
        "static void helper(void) {}\nint main(void) { helper(); return 0; }\n",
    )
    .context("write src/main.c")?;

    let service = start_mcp_server().await?;
    let resp = call_tool(
        &service,
        "meaning_focus",
        serde_json::json!({
            "path": root.path().to_string_lossy(),
            "focus": "src/main.c",
            "query": "outline",
            "max_chars": 4000,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(resp.is_error, Some(true), "meaning_focus returned error");

    let text = resp
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_focus missing text output")?;
    let pack = extract_cp_pack(text)?;
    assert_meaning_invariants(&pack)?;

    anyhow::ensure!(
        pack.contains("S OUTLINE"),
        "expected S OUTLINE section for C file focus"
    );
    anyhow::ensure!(
        pack.lines().any(|line| line.starts_with("SYM kind=fn ")),
        "expected at least one SYM kind=fn line for C file focus"
    );

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
async fn meaning_eval_stub_smoke_dataset_is_high_signal_and_token_efficient() -> Result<()> {
    let repo_root = locate_repo_root()?;
    let dataset_path = repo_root.join("datasets").join("meaning_stub_smoke.json");
    let bytes = std::fs::read(&dataset_path)
        .with_context(|| format!("read meaning dataset {}", dataset_path.display()))?;
    let dataset: MeaningEvalDataset =
        serde_json::from_slice(&bytes).context("parse meaning dataset json")?;
    validate_meaning_dataset(&dataset)?;

    let service = start_mcp_server().await?;

    for case in &dataset.cases {
        let root = tempfile::tempdir().context("temp project dir")?;
        build_fixture(root.path(), &case.fixture)
            .with_context(|| format!("build fixture '{}' (case={})", case.fixture, case.id))?;

        let resp = call_tool(
            &service,
            "meaning_pack",
            serde_json::json!({
                "path": root.path().to_string_lossy(),
                "query": case.query,
                "max_chars": 2000,
                "auto_index": false,
                "response_mode": "facts",
            }),
        )
        .await?;
        assert_ne!(
            resp.is_error,
            Some(true),
            "expected meaning_pack to succeed (case={})",
            case.id
        );

        let text = resp
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .context("meaning_pack missing text output")?;
        let pack = extract_cp_pack(text)?;
        assert_meaning_invariants(&pack)?;

        for path in &case.expect_paths {
            assert!(
                pack.contains(path),
                "expected CP to mention path '{path}' (case={})",
                case.id
            );
        }

        for claim in &case.expect_claims {
            let prefix = format!("{claim} ");
            assert!(
                pack.lines().any(|line| line.starts_with(&prefix)),
                "expected CP to include at least one {claim} claim (case={})",
                case.id
            );
        }

        for kind in &case.expect_anchor_kinds {
            let needle = format!("kind={}", kind.trim());
            assert!(
                pack.lines()
                    .any(|line| line.starts_with("ANCHOR ") && line.contains(&needle)),
                "expected CP to include ANCHOR kind={kind} (case={})",
                case.id
            );
        }

        let needs_dict = case.min_token_saved.is_some() || !case.forbid_map_paths.is_empty();
        let dict = if needs_dict {
            Some(parse_cp_dict(&pack)?)
        } else {
            None
        };

        if !case.forbid_map_paths.is_empty() {
            let dict = dict.as_ref().context("missing dict for forbid_map_paths")?;
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
                    .unwrap_or("");
                let path = dict.get(path_id).cloned().unwrap_or_default();
                for forbid in &case.forbid_map_paths {
                    assert_ne!(
                        path,
                        forbid.as_str(),
                        "expected S MAP to suppress dir '{forbid}' (case={})",
                        case.id
                    );
                }
            }
        }

        if let Some(min_saved) = case.min_token_saved {
            let dict = dict.as_ref().context("missing dict for token_saved")?;
            let mut seen: HashSet<String> = HashSet::new();
            let mut baseline_chars = 0usize;
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
                baseline_chars = baseline_chars.saturating_add(count_file_slice_chars(
                    root.path(),
                    rel,
                    start,
                    end,
                )?);
            }

            let used_chars = pack.chars().count();
            anyhow::ensure!(
                baseline_chars > 0,
                "expected baseline_chars > 0 (case={})",
                case.id
            );
            let token_saved = 1.0 - (used_chars as f64 / baseline_chars as f64);
            anyhow::ensure!(
                token_saved >= min_saved,
                "token_saved regression (case={}, used_chars={}, baseline_chars={}, token_saved={:.4}, min={:.2})",
                case.id,
                used_chars,
                baseline_chars,
                token_saved,
                min_saved
            );
        }
    }

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_pack_is_deterministic_for_same_root_and_query() -> Result<()> {
    let root = tempfile::tempdir().context("temp project dir")?;
    build_fixture_rust_contract_broker_flow(root.path())?;

    let service = start_mcp_server().await?;
    let args = serde_json::json!({
        "path": root.path().to_string_lossy(),
        "query": "determinism check for meaning_pack",
        "max_chars": 2000,
        "auto_index": false,
        "response_mode": "facts",
    });

    let first = call_tool(&service, "meaning_pack", args.clone()).await?;
    assert_ne!(
        first.is_error,
        Some(true),
        "expected meaning_pack to succeed"
    );
    let first_text = first
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_pack missing text output")?;
    let first_pack = extract_cp_pack(first_text)?;
    assert_meaning_invariants(&first_pack)?;

    let second = call_tool(&service, "meaning_pack", args).await?;
    assert_ne!(
        second.is_error,
        Some(true),
        "expected meaning_pack to succeed"
    );
    let second_text = second
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_pack missing text output")?;
    let second_pack = extract_cp_pack(second_text)?;
    assert_meaning_invariants(&second_pack)?;

    assert_eq!(
        first_pack, second_pack,
        "expected meaning_pack output to be deterministic"
    );

    service.cancel().await.context("shutdown mcp service")?;
    Ok(())
}

#[tokio::test]
async fn meaning_pack_does_not_leak_across_roots_in_one_session() -> Result<()> {
    let a_root = tempfile::tempdir().context("temp project dir A")?;
    build_fixture_rust_contract_broker_flow_with_prefix(a_root.path(), "alpha")?;

    let b_root = tempfile::tempdir().context("temp project dir B")?;
    build_fixture_rust_contract_broker_flow_with_prefix(b_root.path(), "beta")?;

    let service = start_mcp_server().await?;

    let a = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": a_root.path().to_string_lossy(),
            "query": "cross-root isolation A",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(a.is_error, Some(true), "expected meaning_pack A to succeed");
    let a_text = a
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_pack A missing text output")?;
    let a_pack = extract_cp_pack(a_text)?;
    assert_meaning_invariants(&a_pack)?;
    assert!(
        a_pack.contains("alpha.created"),
        "expected A pack to mention alpha"
    );
    assert!(
        !a_pack.contains("beta.created"),
        "expected A pack to not mention beta"
    );

    let b = call_tool(
        &service,
        "meaning_pack",
        serde_json::json!({
            "path": b_root.path().to_string_lossy(),
            "query": "cross-root isolation B",
            "max_chars": 2000,
            "auto_index": false,
            "response_mode": "facts",
        }),
    )
    .await?;
    assert_ne!(b.is_error, Some(true), "expected meaning_pack B to succeed");
    let b_text = b
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .context("meaning_pack B missing text output")?;
    let b_pack = extract_cp_pack(b_text)?;
    assert_meaning_invariants(&b_pack)?;
    assert!(
        b_pack.contains("beta.created"),
        "expected B pack to mention beta"
    );
    assert!(
        !b_pack.contains("alpha.created"),
        "expected B pack to not mention alpha"
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
