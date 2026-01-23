use super::candidates::{collect_github_workflow_candidates, is_disallowed_memory_file};
use super::fs_scan::list_immediate_subdirs;
use super::ProjectFactsResult;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

pub(super) const PROJECT_FACTS_VERSION: u32 = 1;

const MAX_FACT_ECOSYSTEMS: usize = 8;
const MAX_FACT_BUILD_TOOLS: usize = 10;
const MAX_FACT_CI: usize = 6;
const MAX_FACT_CONTRACTS: usize = 8;
const MAX_FACT_KEY_DIRS: usize = 12;
const MAX_FACT_MODULES: usize = 16;
const MAX_FACT_ENTRY_POINTS: usize = 10;
const MAX_FACT_KEY_CONFIGS: usize = 20;

fn push_fact(out: &mut Vec<String>, value: &str, max: usize) {
    if out.len() >= max {
        return;
    }
    if out.iter().any(|existing| existing == value) {
        return;
    }
    out.push(value.to_string());
}

fn push_fact_path(out: &mut Vec<String>, root: &Path, rel: &str, max: usize) {
    if out.len() >= max {
        return;
    }
    if is_disallowed_memory_file(rel) {
        return;
    }
    if !root.join(rel).is_file() {
        return;
    }
    push_fact(out, rel, max);
}

fn push_fact_dir(out: &mut Vec<String>, root: &Path, rel: &str, max: usize) {
    if out.len() >= max {
        return;
    }
    if !root.join(rel).is_dir() {
        return;
    }
    push_fact(out, rel, max);
}

fn scan_dir_markers_for_facts(
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

pub(super) fn compute_project_facts(root: &Path) -> ProjectFactsResult {
    let mut ecosystems: Vec<String> = Vec::new();
    let mut build_tools: Vec<String> = Vec::new();
    let mut ci: Vec<String> = Vec::new();
    let mut contracts: Vec<String> = Vec::new();
    let mut key_dirs: Vec<String> = Vec::new();
    let mut modules: Vec<String> = Vec::new();
    let mut entry_points: Vec<String> = Vec::new();
    let mut key_configs: Vec<String> = Vec::new();

    // Root-level file markers (bounded, deterministic).
    let Ok(entries) = fs::read_dir(root) else {
        return ProjectFactsResult {
            version: PROJECT_FACTS_VERSION,
            ecosystems,
            build_tools,
            ci,
            contracts,
            key_dirs,
            modules,
            entry_points,
            key_configs,
        };
    };

    let mut root_files: Vec<String> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let ty = entry.file_type().ok()?;
            if !ty.is_file() {
                return None;
            }
            Some(entry.file_name().to_string_lossy().to_string())
        })
        .collect();
    root_files.sort();

    let has_root_file = |name: &str| root_files.binary_search(&name.to_string()).is_ok();
    let has_any_root_ext = |ext: &str| root_files.iter().any(|name| name.ends_with(ext));

    // Ecosystems.
    if has_root_file("Cargo.toml") {
        push_fact(&mut ecosystems, "rust", MAX_FACT_ECOSYSTEMS);
    }
    if has_root_file("package.json") {
        push_fact(&mut ecosystems, "nodejs", MAX_FACT_ECOSYSTEMS);
        if has_root_file("tsconfig.json") {
            push_fact(&mut ecosystems, "typescript", MAX_FACT_ECOSYSTEMS);
        }
    }
    if has_root_file("pyproject.toml")
        || has_root_file("requirements.txt")
        || has_root_file("setup.py")
        || has_root_file("Pipfile")
    {
        push_fact(&mut ecosystems, "python", MAX_FACT_ECOSYSTEMS);
    }
    if has_root_file("go.mod") {
        push_fact(&mut ecosystems, "go", MAX_FACT_ECOSYSTEMS);
    }
    if has_root_file("pom.xml")
        || has_root_file("build.gradle")
        || has_root_file("build.gradle.kts")
    {
        push_fact(&mut ecosystems, "java", MAX_FACT_ECOSYSTEMS);
    }
    if has_any_root_ext(".sln")
        || has_any_root_ext(".csproj")
        || has_any_root_ext(".fsproj")
        || has_root_file("Directory.Build.props")
        || has_root_file("Directory.Build.targets")
    {
        push_fact(&mut ecosystems, "dotnet", MAX_FACT_ECOSYSTEMS);
    }
    if has_root_file("Gemfile") {
        push_fact(&mut ecosystems, "ruby", MAX_FACT_ECOSYSTEMS);
    }
    if has_root_file("composer.json") {
        push_fact(&mut ecosystems, "php", MAX_FACT_ECOSYSTEMS);
    }

    // Build/task tooling.
    if has_root_file("Cargo.toml") {
        push_fact(&mut build_tools, "cargo", MAX_FACT_BUILD_TOOLS);
    }
    if has_root_file("package.json") {
        if has_root_file("pnpm-lock.yaml") {
            push_fact(&mut build_tools, "pnpm", MAX_FACT_BUILD_TOOLS);
        } else if has_root_file("yarn.lock") {
            push_fact(&mut build_tools, "yarn", MAX_FACT_BUILD_TOOLS);
        } else if has_root_file("bun.lockb") {
            push_fact(&mut build_tools, "bun", MAX_FACT_BUILD_TOOLS);
        } else {
            push_fact(&mut build_tools, "npm", MAX_FACT_BUILD_TOOLS);
        }
    }
    if has_root_file("pyproject.toml") {
        push_fact(&mut build_tools, "pyproject", MAX_FACT_BUILD_TOOLS);
    }
    if has_root_file("poetry.lock") {
        push_fact(&mut build_tools, "poetry", MAX_FACT_BUILD_TOOLS);
    }
    if has_root_file("Makefile") {
        push_fact(&mut build_tools, "make", MAX_FACT_BUILD_TOOLS);
    }
    if has_root_file("CMakeLists.txt") {
        push_fact(&mut build_tools, "cmake", MAX_FACT_BUILD_TOOLS);
    }
    if has_root_file("WORKSPACE") || has_root_file("WORKSPACE.bazel") {
        push_fact(&mut build_tools, "bazel", MAX_FACT_BUILD_TOOLS);
    }
    if has_root_file("flake.nix") || has_root_file("default.nix") {
        push_fact(&mut build_tools, "nix", MAX_FACT_BUILD_TOOLS);
    }
    if has_root_file("justfile") || has_root_file("Justfile") {
        push_fact(&mut build_tools, "just", MAX_FACT_BUILD_TOOLS);
    }

    // CI/CD tooling.
    if root.join(".github").join("workflows").is_dir() {
        push_fact(&mut ci, "github_actions", MAX_FACT_CI);
    }
    if has_root_file(".gitlab-ci.yml") {
        push_fact(&mut ci, "gitlab_ci", MAX_FACT_CI);
    }
    if root.join(".circleci").is_dir() {
        push_fact(&mut ci, "circleci", MAX_FACT_CI);
    }
    if has_root_file("azure-pipelines.yml") || has_root_file("azure-pipelines.yaml") {
        push_fact(&mut ci, "azure_pipelines", MAX_FACT_CI);
    }
    if has_root_file(".travis.yml") {
        push_fact(&mut ci, "travis_ci", MAX_FACT_CI);
    }

    // Contract surfaces.
    push_fact_dir(&mut contracts, root, "contracts", MAX_FACT_CONTRACTS);
    push_fact_dir(&mut contracts, root, "proto", MAX_FACT_CONTRACTS);
    push_fact_path(
        &mut contracts,
        root,
        "contracts/http/v1/openapi.json",
        MAX_FACT_CONTRACTS,
    );
    push_fact_path(
        &mut contracts,
        root,
        "contracts/http/openapi.json",
        MAX_FACT_CONTRACTS,
    );
    push_fact_path(&mut contracts, root, "openapi.json", MAX_FACT_CONTRACTS);
    push_fact_path(&mut contracts, root, "openapi.yaml", MAX_FACT_CONTRACTS);
    push_fact_path(&mut contracts, root, "openapi.yml", MAX_FACT_CONTRACTS);
    push_fact_path(
        &mut contracts,
        root,
        "proto/command.proto",
        MAX_FACT_CONTRACTS,
    );

    // Key top-level directories (agent navigation map, bounded).
    // Prefer a priority-ordered listing of *existing* directories over a fixed list: this keeps
    // project_facts useful across arbitrary repo topologies without hardcoding per-project rules.
    for rel in list_immediate_subdirs(root, MAX_FACT_KEY_DIRS) {
        push_fact_dir(&mut key_dirs, root, &rel, MAX_FACT_KEY_DIRS);
    }

    // Topology scan (bounded): if the project root is a wrapper (monorepo container, nested repo),
    // detect marker files in immediate subdirectories and surface them as modules + facts.
    for name in list_immediate_subdirs(root, 24) {
        if modules.len() >= MAX_FACT_MODULES {
            break;
        }
        scan_dir_markers_for_facts(
            &mut ecosystems,
            &mut build_tools,
            &mut modules,
            &mut entry_points,
            &mut key_configs,
            root,
            &name,
        );
    }

    // Workspace / module roots (monorepos), bounded + deterministic.
    const MAX_CONTAINER_SUBDIRS: usize = 24;
    for container in ["crates", "packages", "apps", "services", "libs", "lib"] {
        if modules.len() >= MAX_FACT_MODULES && entry_points.len() >= MAX_FACT_ENTRY_POINTS {
            break;
        }

        let dir = root.join(container);
        if !dir.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };

        let mut candidates: Vec<String> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                if !entry.file_type().ok()?.is_dir() {
                    return None;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if name.is_empty() {
                    return None;
                }
                // Skip obvious noise inside monorepo containers.
                if name == "target"
                    || name == "node_modules"
                    || name == ".context"
                    || name == ".context-finder"
                {
                    return None;
                }
                Some(name)
            })
            .collect();

        candidates.sort();
        candidates.truncate(MAX_CONTAINER_SUBDIRS);

        for name in candidates {
            if modules.len() >= MAX_FACT_MODULES {
                break;
            }
            let rel = format!("{container}/{name}");
            scan_dir_markers_for_facts(
                &mut ecosystems,
                &mut build_tools,
                &mut modules,
                &mut entry_points,
                &mut key_configs,
                root,
                &rel,
            );
        }
    }

    // Wrapper → nested repo fallback (depth-2, bounded).
    let looks_like_wrapper = ecosystems.is_empty() && modules.is_empty() && entry_points.is_empty();
    if looks_like_wrapper {
        for outer in list_immediate_subdirs(root, 8) {
            if modules.len() >= MAX_FACT_MODULES {
                break;
            }
            let outer_root = root.join(&outer);
            if !outer_root.is_dir() {
                continue;
            }
            for inner in list_immediate_subdirs(&outer_root, 8) {
                if modules.len() >= MAX_FACT_MODULES {
                    break;
                }
                let rel = format!("{outer}/{inner}");
                scan_dir_markers_for_facts(
                    &mut ecosystems,
                    &mut build_tools,
                    &mut modules,
                    &mut entry_points,
                    &mut key_configs,
                    root,
                    &rel,
                );
            }
        }
    }

    // Go-style cmd/* entrypoints as modules.
    if modules.len() < MAX_FACT_MODULES && root.join("cmd").is_dir() {
        if let Ok(entries) = fs::read_dir(root.join("cmd")) {
            let mut cmd_dirs: Vec<String> = entries
                .filter_map(|entry| {
                    let entry = entry.ok()?;
                    if !entry.file_type().ok()?.is_dir() {
                        return None;
                    }
                    let name = entry.file_name().to_string_lossy().to_string();
                    let rel = format!("cmd/{name}");
                    if root.join(&rel).join("main.go").is_file() {
                        Some(rel)
                    } else {
                        None
                    }
                })
                .collect();
            cmd_dirs.sort();
            for rel in cmd_dirs {
                push_fact(&mut modules, &rel, MAX_FACT_MODULES);
                if modules.len() >= MAX_FACT_MODULES {
                    break;
                }
            }
        }
    }

    // Entrypoint candidates.
    push_fact_path(
        &mut entry_points,
        root,
        "src/main.rs",
        MAX_FACT_ENTRY_POINTS,
    );
    push_fact_path(&mut entry_points, root, "src/lib.rs", MAX_FACT_ENTRY_POINTS);
    push_fact_path(&mut entry_points, root, "main.go", MAX_FACT_ENTRY_POINTS);
    push_fact_path(&mut entry_points, root, "main.py", MAX_FACT_ENTRY_POINTS);
    push_fact_path(&mut entry_points, root, "app.py", MAX_FACT_ENTRY_POINTS);
    push_fact_path(
        &mut entry_points,
        root,
        "src/main.py",
        MAX_FACT_ENTRY_POINTS,
    );
    push_fact_path(&mut entry_points, root, "src/app.py", MAX_FACT_ENTRY_POINTS);
    push_fact_path(
        &mut entry_points,
        root,
        "src/index.ts",
        MAX_FACT_ENTRY_POINTS,
    );
    push_fact_path(
        &mut entry_points,
        root,
        "src/index.js",
        MAX_FACT_ENTRY_POINTS,
    );
    push_fact_path(&mut entry_points, root, "index.ts", MAX_FACT_ENTRY_POINTS);
    push_fact_path(&mut entry_points, root, "index.js", MAX_FACT_ENTRY_POINTS);
    push_fact_path(
        &mut entry_points,
        root,
        "src/server.ts",
        MAX_FACT_ENTRY_POINTS,
    );
    push_fact_path(
        &mut entry_points,
        root,
        "src/server.js",
        MAX_FACT_ENTRY_POINTS,
    );
    push_fact_path(
        &mut entry_points,
        root,
        "src/main.ts",
        MAX_FACT_ENTRY_POINTS,
    );
    push_fact_path(
        &mut entry_points,
        root,
        "src/main.js",
        MAX_FACT_ENTRY_POINTS,
    );
    push_fact_path(
        &mut entry_points,
        root,
        "cmd/main.go",
        MAX_FACT_ENTRY_POINTS,
    );
    if entry_points.len() < MAX_FACT_ENTRY_POINTS && root.join("cmd").is_dir() {
        if let Ok(entries) = fs::read_dir(root.join("cmd")) {
            let mut cmd_mains: Vec<String> = entries
                .filter_map(|entry| {
                    let entry = entry.ok()?;
                    let ty = entry.file_type().ok()?;
                    if !ty.is_dir() {
                        return None;
                    }
                    let name = entry.file_name().to_string_lossy().to_string();
                    let rel = format!("cmd/{name}/main.go");
                    if root.join(&rel).is_file() {
                        Some(rel)
                    } else {
                        None
                    }
                })
                .collect();
            cmd_mains.sort();
            for rel in cmd_mains {
                push_fact_path(&mut entry_points, root, &rel, MAX_FACT_ENTRY_POINTS);
            }
        }
    }

    // Key config files worth reading first (safe allowlist, bounded, agent-signal oriented).
    push_fact_path(
        &mut key_configs,
        root,
        ".tool-versions",
        MAX_FACT_KEY_CONFIGS,
    );
    push_fact_path(
        &mut key_configs,
        root,
        ".editorconfig",
        MAX_FACT_KEY_CONFIGS,
    );
    push_fact_path(&mut key_configs, root, "Makefile", MAX_FACT_KEY_CONFIGS);
    push_fact_path(&mut key_configs, root, "Justfile", MAX_FACT_KEY_CONFIGS);
    push_fact_path(&mut key_configs, root, "justfile", MAX_FACT_KEY_CONFIGS);

    push_fact_path(&mut key_configs, root, "Cargo.toml", MAX_FACT_KEY_CONFIGS);
    push_fact_path(
        &mut key_configs,
        root,
        "rust-toolchain.toml",
        MAX_FACT_KEY_CONFIGS,
    );
    push_fact_path(
        &mut key_configs,
        root,
        "rust-toolchain",
        MAX_FACT_KEY_CONFIGS,
    );

    push_fact_path(&mut key_configs, root, "package.json", MAX_FACT_KEY_CONFIGS);
    push_fact_path(
        &mut key_configs,
        root,
        "tsconfig.json",
        MAX_FACT_KEY_CONFIGS,
    );
    push_fact_path(
        &mut key_configs,
        root,
        "pyproject.toml",
        MAX_FACT_KEY_CONFIGS,
    );
    push_fact_path(
        &mut key_configs,
        root,
        "requirements.txt",
        MAX_FACT_KEY_CONFIGS,
    );
    push_fact_path(&mut key_configs, root, "go.mod", MAX_FACT_KEY_CONFIGS);

    push_fact_path(&mut key_configs, root, "Dockerfile", MAX_FACT_KEY_CONFIGS);
    push_fact_path(
        &mut key_configs,
        root,
        "docker-compose.yml",
        MAX_FACT_KEY_CONFIGS,
    );
    push_fact_path(
        &mut key_configs,
        root,
        "docker-compose.yaml",
        MAX_FACT_KEY_CONFIGS,
    );

    push_fact_path(&mut key_configs, root, ".nvmrc", MAX_FACT_KEY_CONFIGS);
    push_fact_path(
        &mut key_configs,
        root,
        ".node-version",
        MAX_FACT_KEY_CONFIGS,
    );
    push_fact_path(
        &mut key_configs,
        root,
        ".python-version",
        MAX_FACT_KEY_CONFIGS,
    );
    push_fact_path(
        &mut key_configs,
        root,
        ".ruby-version",
        MAX_FACT_KEY_CONFIGS,
    );

    push_fact_path(&mut key_configs, root, ".env.example", MAX_FACT_KEY_CONFIGS);
    push_fact_path(&mut key_configs, root, ".env.sample", MAX_FACT_KEY_CONFIGS);
    push_fact_path(
        &mut key_configs,
        root,
        ".env.template",
        MAX_FACT_KEY_CONFIGS,
    );
    push_fact_path(&mut key_configs, root, ".env.dist", MAX_FACT_KEY_CONFIGS);

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
                &mut key_configs,
                root,
                &format!("{dir}/{name}"),
                MAX_FACT_KEY_CONFIGS,
            );
        }
    }

    push_fact_path(&mut key_configs, root, "flake.nix", MAX_FACT_KEY_CONFIGS);
    push_fact_path(
        &mut key_configs,
        root,
        "CMakeLists.txt",
        MAX_FACT_KEY_CONFIGS,
    );
    push_fact_path(&mut key_configs, root, ".gitignore", MAX_FACT_KEY_CONFIGS);

    if key_configs.len() < MAX_FACT_KEY_CONFIGS {
        let mut seen = HashSet::new();
        let workflows = collect_github_workflow_candidates(root, &mut seen);
        for rel in workflows {
            push_fact_path(&mut key_configs, root, &rel, MAX_FACT_KEY_CONFIGS);
        }
    }

    ProjectFactsResult {
        version: PROJECT_FACTS_VERSION,
        ecosystems,
        build_tools,
        ci,
        contracts,
        key_dirs,
        modules,
        entry_points,
        key_configs,
    }
}
