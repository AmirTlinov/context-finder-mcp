use std::path::Path;

mod doc;
mod memory;
mod ops;

pub(super) const DEFAULT_MEMORY_FILE_CANDIDATES: &[&str] = &[
    "AGENTS.md",
    "AGENTS.context",
    "README.md",
    "README.context",
    "docs/README.md",
    "docs/README.context",
    "docs/QUICK_START.md",
    "docs/QUICK_START.context",
    "ARCHITECTURE.md",
    "ARCHITECTURE.context",
    "docs/ARCHITECTURE.md",
    "docs/ARCHITECTURE.context",
    "GOALS.md",
    "GOALS.context",
    "docs/GOALS.md",
    "docs/GOALS.context",
    "PHILOSOPHY.md",
    "PHILOSOPHY.context",
    "CONTRIBUTING.md",
    "DEVELOPMENT.md",
    "docs/DEVELOPMENT.md",
    "tests/README.md",
    ".env.example",
    ".env.sample",
    ".env.template",
    ".env.dist",
    ".nvmrc",
    ".node-version",
    ".python-version",
    ".ruby-version",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "requirements.txt",
    "Makefile",
    "Justfile",
    "justfile",
    "rust-toolchain.toml",
    "rust-toolchain",
    "tsconfig.json",
    "Dockerfile",
    "docker-compose.yml",
    ".gitlab-ci.yml",
    ".editorconfig",
    ".tool-versions",
    ".devcontainer/devcontainer.json",
    ".vscode/tasks.json",
    ".vscode/launch.json",
    ".vscode/settings.json",
    ".vscode/extensions.json",
];

const MODULE_MEMORY_FILE_CANDIDATES: &[&str] = &[
    // Docs first (these drive the "how do I run/test/deploy" UX).
    "AGENTS.md",
    "AGENTS.context",
    "README.md",
    "README.context",
    "docs/README.md",
    "docs/README.context",
    "docs/QUICK_START.md",
    "docs/QUICK_START.context",
    "ARCHITECTURE.md",
    "ARCHITECTURE.context",
    "docs/ARCHITECTURE.md",
    "docs/ARCHITECTURE.context",
    "GOALS.md",
    "GOALS.context",
    "docs/GOALS.md",
    "docs/GOALS.context",
    "PHILOSOPHY.md",
    "PHILOSOPHY.context",
    "CONTRIBUTING.md",
    "DEVELOPMENT.md",
    "docs/DEVELOPMENT.md",
    "tests/README.md",
    // Config hints (bounded, high-signal).
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "requirements.txt",
    "Makefile",
    "Justfile",
    "justfile",
    "rust-toolchain.toml",
    "rust-toolchain",
    "tsconfig.json",
    "Dockerfile",
    "docker-compose.yml",
    ".gitlab-ci.yml",
    ".editorconfig",
];

pub(super) use doc::collect_github_workflow_candidates;
pub(super) use memory::{collect_memory_file_candidates, config_candidate_score};
pub(super) use ops::{collect_ops_file_candidates, ops_candidate_score};

pub(super) fn is_disallowed_memory_file(candidate: &str) -> bool {
    let rel = candidate.trim().replace('\\', "/");
    if rel == ".agents" || rel.starts_with(".agents/") {
        return true;
    }
    crate::tools::secrets::is_potential_secret_path(&rel)
}

fn is_doc_memory_candidate(rel: &str) -> bool {
    Path::new(rel)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "md" | "mdx" | "rst" | "adoc" | "txt" | "context"
            )
        })
}
