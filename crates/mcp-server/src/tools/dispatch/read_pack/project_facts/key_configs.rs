use super::super::candidates::collect_github_workflow_candidates;
use super::limits::{push_fact_path, MAX_FACT_KEY_CONFIGS};
use std::collections::HashSet;
use std::path::Path;

pub(super) fn append_key_configs(root: &Path, key_configs: &mut Vec<String>) {
    // Key config files worth reading first (safe allowlist, bounded, agent-signal oriented).
    push_fact_path(key_configs, root, ".tool-versions", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, ".editorconfig", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, "Makefile", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, "Justfile", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, "justfile", MAX_FACT_KEY_CONFIGS);

    push_fact_path(key_configs, root, "Cargo.toml", MAX_FACT_KEY_CONFIGS);
    push_fact_path(
        key_configs,
        root,
        "rust-toolchain.toml",
        MAX_FACT_KEY_CONFIGS,
    );
    push_fact_path(key_configs, root, "rust-toolchain", MAX_FACT_KEY_CONFIGS);

    push_fact_path(key_configs, root, "package.json", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, "tsconfig.json", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, "pyproject.toml", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, "requirements.txt", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, "go.mod", MAX_FACT_KEY_CONFIGS);

    push_fact_path(key_configs, root, "Dockerfile", MAX_FACT_KEY_CONFIGS);
    push_fact_path(
        key_configs,
        root,
        "docker-compose.yml",
        MAX_FACT_KEY_CONFIGS,
    );
    push_fact_path(
        key_configs,
        root,
        "docker-compose.yaml",
        MAX_FACT_KEY_CONFIGS,
    );

    push_fact_path(key_configs, root, ".nvmrc", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, ".node-version", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, ".python-version", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, ".ruby-version", MAX_FACT_KEY_CONFIGS);

    push_fact_path(key_configs, root, ".env.example", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, ".env.sample", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, ".env.template", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, ".env.dist", MAX_FACT_KEY_CONFIGS);

    // Common nested config locations: keep this bounded and filename-based (no globbing).
    for dir in ["config", "configs"] {
        if key_configs.len() >= MAX_FACT_KEY_CONFIGS {
            break;
        }
        if !root.join(dir).is_dir() {
            continue;
        }
        for name in [
            "docker-compose.yml",
            "docker-compose.yaml",
            "Dockerfile",
            ".env.example",
            ".env.sample",
            ".env.template",
            ".env.dist",
            "config.yml",
            "config.yaml",
            "settings.yml",
            "settings.yaml",
            "application.yml",
            "application.yaml",
        ] {
            if key_configs.len() >= MAX_FACT_KEY_CONFIGS {
                break;
            }
            push_fact_path(
                key_configs,
                root,
                &format!("{dir}/{name}"),
                MAX_FACT_KEY_CONFIGS,
            );
        }
    }

    push_fact_path(key_configs, root, "flake.nix", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, "CMakeLists.txt", MAX_FACT_KEY_CONFIGS);
    push_fact_path(key_configs, root, ".gitignore", MAX_FACT_KEY_CONFIGS);

    if key_configs.len() < MAX_FACT_KEY_CONFIGS {
        let mut seen = HashSet::new();
        let workflows = collect_github_workflow_candidates(root, &mut seen);
        for rel in workflows {
            push_fact_path(key_configs, root, &rel, MAX_FACT_KEY_CONFIGS);
        }
    }
}
