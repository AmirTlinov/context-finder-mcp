use super::limits::{
    push_fact, push_fact_path, MAX_FACT_BUILD_TOOLS, MAX_FACT_ECOSYSTEMS, MAX_FACT_ENTRY_POINTS,
    MAX_FACT_KEY_CONFIGS, MAX_FACT_MODULES,
};
use std::path::Path;

pub(super) fn scan_dir_markers_for_facts(
    ecosystems: &mut Vec<String>,
    build_tools: &mut Vec<String>,
    modules: &mut Vec<String>,
    entry_points: &mut Vec<String>,
    key_configs: &mut Vec<String>,
    root: &Path,
    rel: &str,
) {
    let module_root = root.join(rel);
    if !module_root.is_dir() {
        return;
    }

    let file_exists = |name: &str| module_root.join(name).is_file();

    if file_exists("Cargo.toml") {
        push_fact(ecosystems, "rust", MAX_FACT_ECOSYSTEMS);
        push_fact(build_tools, "cargo", MAX_FACT_BUILD_TOOLS);
        push_fact(modules, rel, MAX_FACT_MODULES);
        push_fact_path(
            key_configs,
            root,
            &format!("{rel}/Cargo.toml"),
            MAX_FACT_KEY_CONFIGS,
        );
        push_fact_path(
            entry_points,
            root,
            &format!("{rel}/src/main.rs"),
            MAX_FACT_ENTRY_POINTS,
        );
        push_fact_path(
            entry_points,
            root,
            &format!("{rel}/src/lib.rs"),
            MAX_FACT_ENTRY_POINTS,
        );
    }

    if file_exists("package.json") {
        push_fact(ecosystems, "nodejs", MAX_FACT_ECOSYSTEMS);
        if file_exists("tsconfig.json") {
            push_fact(ecosystems, "typescript", MAX_FACT_ECOSYSTEMS);
        }
        // Best-effort package manager hint from lockfiles (bounded).
        if file_exists("pnpm-lock.yaml") {
            push_fact(build_tools, "pnpm", MAX_FACT_BUILD_TOOLS);
        } else if file_exists("yarn.lock") {
            push_fact(build_tools, "yarn", MAX_FACT_BUILD_TOOLS);
        } else if file_exists("bun.lockb") {
            push_fact(build_tools, "bun", MAX_FACT_BUILD_TOOLS);
        } else {
            push_fact(build_tools, "npm", MAX_FACT_BUILD_TOOLS);
        }
        push_fact(modules, rel, MAX_FACT_MODULES);
        push_fact_path(
            key_configs,
            root,
            &format!("{rel}/package.json"),
            MAX_FACT_KEY_CONFIGS,
        );
        push_fact_path(
            key_configs,
            root,
            &format!("{rel}/tsconfig.json"),
            MAX_FACT_KEY_CONFIGS,
        );
        push_fact_path(
            entry_points,
            root,
            &format!("{rel}/src/index.ts"),
            MAX_FACT_ENTRY_POINTS,
        );
        push_fact_path(
            entry_points,
            root,
            &format!("{rel}/src/index.js"),
            MAX_FACT_ENTRY_POINTS,
        );
        push_fact_path(
            entry_points,
            root,
            &format!("{rel}/src/main.ts"),
            MAX_FACT_ENTRY_POINTS,
        );
        push_fact_path(
            entry_points,
            root,
            &format!("{rel}/src/main.js"),
            MAX_FACT_ENTRY_POINTS,
        );
    }

    if file_exists("pyproject.toml")
        || file_exists("requirements.txt")
        || file_exists("setup.py")
        || file_exists("Pipfile")
    {
        push_fact(ecosystems, "python", MAX_FACT_ECOSYSTEMS);
        push_fact(modules, rel, MAX_FACT_MODULES);
        push_fact_path(
            key_configs,
            root,
            &format!("{rel}/pyproject.toml"),
            MAX_FACT_KEY_CONFIGS,
        );
        push_fact_path(
            key_configs,
            root,
            &format!("{rel}/requirements.txt"),
            MAX_FACT_KEY_CONFIGS,
        );
        push_fact_path(
            entry_points,
            root,
            &format!("{rel}/app.py"),
            MAX_FACT_ENTRY_POINTS,
        );
        push_fact_path(
            entry_points,
            root,
            &format!("{rel}/src/main.py"),
            MAX_FACT_ENTRY_POINTS,
        );
        push_fact_path(
            entry_points,
            root,
            &format!("{rel}/src/app.py"),
            MAX_FACT_ENTRY_POINTS,
        );
    }

    if file_exists("go.mod") {
        push_fact(ecosystems, "go", MAX_FACT_ECOSYSTEMS);
        push_fact(build_tools, "go", MAX_FACT_BUILD_TOOLS);
        push_fact(modules, rel, MAX_FACT_MODULES);
        push_fact_path(
            key_configs,
            root,
            &format!("{rel}/go.mod"),
            MAX_FACT_KEY_CONFIGS,
        );
        push_fact_path(
            entry_points,
            root,
            &format!("{rel}/main.go"),
            MAX_FACT_ENTRY_POINTS,
        );
    }

    if file_exists("Makefile") {
        push_fact(build_tools, "make", MAX_FACT_BUILD_TOOLS);
        push_fact_path(
            key_configs,
            root,
            &format!("{rel}/Makefile"),
            MAX_FACT_KEY_CONFIGS,
        );
    }

    if file_exists("Justfile") || file_exists("justfile") {
        push_fact(build_tools, "just", MAX_FACT_BUILD_TOOLS);
        push_fact_path(
            key_configs,
            root,
            &format!("{rel}/Justfile"),
            MAX_FACT_KEY_CONFIGS,
        );
        push_fact_path(
            key_configs,
            root,
            &format!("{rel}/justfile"),
            MAX_FACT_KEY_CONFIGS,
        );
    }
}
