use super::candidates::{config_candidate_score, is_disallowed_memory_file};
use super::recall::{recall_doc_candidate_score, RecallStructuralIntent};
use super::{entrypoint_candidate_score, ProjectFactsResult};
use std::collections::HashSet;
use std::path::Path;

fn contract_candidate_score(rel: &str) -> i32 {
    let normalized = rel.replace('\\', "/").to_ascii_lowercase();
    match normalized.as_str() {
        "docs/contracts/protocol.md" => 300,
        "docs/contracts/readme.md" => 280,
        "contracts/http/v1/openapi.json" => 260,
        "contracts/http/v1/openapi.yaml" | "contracts/http/v1/openapi.yml" => 255,
        "openapi.json" | "openapi.yaml" | "openapi.yml" => 250,
        "proto/command.proto" => 240,
        "architecture.md" | "docs/architecture.md" => 220,
        "readme.md" => 210,
        _ if normalized.starts_with("docs/contracts/") && normalized.ends_with(".md") => 200,
        _ if normalized.starts_with("contracts/") => 180,
        _ if normalized.starts_with("proto/") && normalized.ends_with(".proto") => 170,
        _ => 10,
    }
}

pub(super) fn recall_structural_candidates(
    intent: RecallStructuralIntent,
    root: &Path,
    facts: &ProjectFactsResult,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = HashSet::new();

    let mut push = |rel: &str| {
        let rel = rel.trim().replace('\\', "/");
        if rel.is_empty() || rel == "." {
            return;
        }
        if is_disallowed_memory_file(&rel) {
            return;
        }
        if !root.join(&rel).is_file() {
            return;
        }
        if seen.insert(rel.clone()) {
            out.push(rel);
        }
    };

    match intent {
        RecallStructuralIntent::ProjectIdentity => {
            for rel in [
                "README.md",
                "docs/README.md",
                "AGENTS.md",
                "PHILOSOPHY.md",
                "ARCHITECTURE.md",
                "docs/ARCHITECTURE.md",
                "docs/QUICK_START.md",
                "DEVELOPMENT.md",
                "CONTRIBUTING.md",
            ] {
                push(rel);
            }

            // If the root is a wrapper, surface module docs as well (bounded, deterministic).
            for module in facts.modules.iter().take(6) {
                for rel in ["README.md", "AGENTS.md", "docs/README.md"] {
                    push(&format!("{module}/{rel}"));
                }
            }

            out.sort_by(|a, b| {
                recall_doc_candidate_score(b)
                    .cmp(&recall_doc_candidate_score(a))
                    .then_with(|| a.cmp(b))
            });
        }
        RecallStructuralIntent::EntryPoints => {
            // Start with manifest-level hints, then actual code entrypoints.
            for rel in [
                "Cargo.toml",
                "package.json",
                "pyproject.toml",
                "go.mod",
                "README.md",
            ] {
                push(rel);
            }

            for rel in &facts.entry_points {
                push(rel);
            }

            // If project_facts didn't find module entrypoints, derive a few from module roots.
            for module in facts.modules.iter().take(12) {
                for rel in [
                    "src/main.rs",
                    "src/lib.rs",
                    "main.go",
                    "main.py",
                    "app.py",
                    "src/main.py",
                    "src/app.py",
                    "src/index.ts",
                    "src/index.js",
                    "src/main.ts",
                    "src/main.js",
                ] {
                    push(&format!("{module}/{rel}"));
                }
            }

            out.sort_by(|a, b| {
                entrypoint_candidate_score(b)
                    .cmp(&entrypoint_candidate_score(a))
                    .then_with(|| a.cmp(b))
            });
        }
        RecallStructuralIntent::Contracts => {
            for rel in [
                "docs/contracts/protocol.md",
                "docs/contracts/README.md",
                "docs/contracts/runtime.md",
                "docs/contracts/quality_gates.md",
                "ARCHITECTURE.md",
                "docs/ARCHITECTURE.md",
                "README.md",
                "proto/command.proto",
                "contracts/http/v1/openapi.json",
                "contracts/http/v1/openapi.yaml",
                "contracts/http/v1/openapi.yml",
                "openapi.json",
                "openapi.yaml",
                "openapi.yml",
            ] {
                push(rel);
            }

            // If there are contract dirs, surface one or two stable "front door" docs from them.
            for module in facts
                .contracts
                .iter()
                .filter(|c| c.ends_with('/') || root.join(c).is_dir())
                .take(4)
            {
                for rel in ["README.md", "readme.md"] {
                    push(&format!("{module}/{rel}"));
                }
            }

            out.sort_by(|a, b| {
                contract_candidate_score(b)
                    .cmp(&contract_candidate_score(a))
                    .then_with(|| a.cmp(b))
            });
        }
        RecallStructuralIntent::Configuration => {
            // Doc hints first (what config is used), then the concrete config files.
            for rel in ["README.md", "docs/QUICK_START.md", "DEVELOPMENT.md"] {
                push(rel);
            }

            for rel in &facts.key_configs {
                push(rel);
            }

            for rel in [
                "config/.env.example",
                "config/.env.sample",
                "config/.env.template",
                "config/.env.dist",
                "config/docker-compose.yml",
                "config/docker-compose.yaml",
                "configs/.env.example",
                "configs/docker-compose.yml",
                "configs/docker-compose.yaml",
                "config/config.yml",
                "config/config.yaml",
                "config/settings.yml",
                "config/settings.yaml",
                "configs/config.yml",
                "configs/config.yaml",
                "configs/settings.yml",
                "configs/settings.yaml",
            ] {
                push(rel);
            }

            out.sort_by(|a, b| {
                config_candidate_score(b)
                    .cmp(&config_candidate_score(a))
                    .then_with(|| a.cmp(b))
            });
        }
    }

    out
}
