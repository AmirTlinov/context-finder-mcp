use super::limits::{
    push_fact, push_fact_dir, push_fact_path, MAX_FACT_BUILD_TOOLS, MAX_FACT_CI,
    MAX_FACT_CONTRACTS, MAX_FACT_ECOSYSTEMS,
};
use std::fs;
use std::path::Path;

pub(super) struct RootFiles {
    names: Vec<String>,
}

impl RootFiles {
    pub(super) fn read(root: &Path) -> Option<Self> {
        let entries = fs::read_dir(root).ok()?;
        let mut names: Vec<String> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let ty = entry.file_type().ok()?;
                if !ty.is_file() {
                    return None;
                }
                Some(entry.file_name().to_string_lossy().to_string())
            })
            .collect();
        names.sort();
        Some(Self { names })
    }

    pub(super) fn has_file(&self, name: &str) -> bool {
        self.names.binary_search(&name.to_string()).is_ok()
    }

    pub(super) fn has_any_ext(&self, ext: &str) -> bool {
        self.names.iter().any(|name| name.ends_with(ext))
    }
}

pub(super) fn apply_root_ecosystems(root_files: &RootFiles, ecosystems: &mut Vec<String>) {
    if root_files.has_file("Cargo.toml") {
        push_fact(ecosystems, "rust", MAX_FACT_ECOSYSTEMS);
    }
    if root_files.has_file("package.json") {
        push_fact(ecosystems, "nodejs", MAX_FACT_ECOSYSTEMS);
        if root_files.has_file("tsconfig.json") {
            push_fact(ecosystems, "typescript", MAX_FACT_ECOSYSTEMS);
        }
    }
    if root_files.has_file("pyproject.toml")
        || root_files.has_file("requirements.txt")
        || root_files.has_file("setup.py")
        || root_files.has_file("Pipfile")
    {
        push_fact(ecosystems, "python", MAX_FACT_ECOSYSTEMS);
    }
    if root_files.has_file("go.mod") {
        push_fact(ecosystems, "go", MAX_FACT_ECOSYSTEMS);
    }
    if root_files.has_file("pom.xml")
        || root_files.has_file("build.gradle")
        || root_files.has_file("build.gradle.kts")
    {
        push_fact(ecosystems, "java", MAX_FACT_ECOSYSTEMS);
    }
    if root_files.has_any_ext(".sln")
        || root_files.has_any_ext(".csproj")
        || root_files.has_any_ext(".fsproj")
        || root_files.has_file("Directory.Build.props")
        || root_files.has_file("Directory.Build.targets")
    {
        push_fact(ecosystems, "dotnet", MAX_FACT_ECOSYSTEMS);
    }
    if root_files.has_file("Gemfile") {
        push_fact(ecosystems, "ruby", MAX_FACT_ECOSYSTEMS);
    }
    if root_files.has_file("composer.json") {
        push_fact(ecosystems, "php", MAX_FACT_ECOSYSTEMS);
    }
}

pub(super) fn apply_root_build_tools(root_files: &RootFiles, build_tools: &mut Vec<String>) {
    if root_files.has_file("Cargo.toml") {
        push_fact(build_tools, "cargo", MAX_FACT_BUILD_TOOLS);
    }
    if root_files.has_file("package.json") {
        if root_files.has_file("pnpm-lock.yaml") {
            push_fact(build_tools, "pnpm", MAX_FACT_BUILD_TOOLS);
        } else if root_files.has_file("yarn.lock") {
            push_fact(build_tools, "yarn", MAX_FACT_BUILD_TOOLS);
        } else if root_files.has_file("bun.lockb") {
            push_fact(build_tools, "bun", MAX_FACT_BUILD_TOOLS);
        } else {
            push_fact(build_tools, "npm", MAX_FACT_BUILD_TOOLS);
        }
    }
    if root_files.has_file("pyproject.toml") {
        push_fact(build_tools, "pyproject", MAX_FACT_BUILD_TOOLS);
    }
    if root_files.has_file("poetry.lock") {
        push_fact(build_tools, "poetry", MAX_FACT_BUILD_TOOLS);
    }
    if root_files.has_file("Makefile") {
        push_fact(build_tools, "make", MAX_FACT_BUILD_TOOLS);
    }
    if root_files.has_file("CMakeLists.txt") {
        push_fact(build_tools, "cmake", MAX_FACT_BUILD_TOOLS);
    }
    if root_files.has_file("WORKSPACE") || root_files.has_file("WORKSPACE.bazel") {
        push_fact(build_tools, "bazel", MAX_FACT_BUILD_TOOLS);
    }
    if root_files.has_file("flake.nix") || root_files.has_file("default.nix") {
        push_fact(build_tools, "nix", MAX_FACT_BUILD_TOOLS);
    }
    if root_files.has_file("justfile") || root_files.has_file("Justfile") {
        push_fact(build_tools, "just", MAX_FACT_BUILD_TOOLS);
    }
}

pub(super) fn apply_root_ci(root: &Path, root_files: &RootFiles, ci: &mut Vec<String>) {
    if root.join(".github").join("workflows").is_dir() {
        push_fact(ci, "github_actions", MAX_FACT_CI);
    }
    if root_files.has_file(".gitlab-ci.yml") {
        push_fact(ci, "gitlab_ci", MAX_FACT_CI);
    }
    if root.join(".circleci").is_dir() {
        push_fact(ci, "circleci", MAX_FACT_CI);
    }
    if root_files.has_file("azure-pipelines.yml") || root_files.has_file("azure-pipelines.yaml") {
        push_fact(ci, "azure_pipelines", MAX_FACT_CI);
    }
    if root_files.has_file(".travis.yml") {
        push_fact(ci, "travis_ci", MAX_FACT_CI);
    }
}

pub(super) fn apply_contracts(root: &Path, contracts: &mut Vec<String>) {
    push_fact_dir(contracts, root, "contracts", MAX_FACT_CONTRACTS);
    push_fact_dir(contracts, root, "proto", MAX_FACT_CONTRACTS);
    push_fact_path(
        contracts,
        root,
        "contracts/http/v1/openapi.json",
        MAX_FACT_CONTRACTS,
    );
    push_fact_path(
        contracts,
        root,
        "contracts/http/openapi.json",
        MAX_FACT_CONTRACTS,
    );
    push_fact_path(contracts, root, "openapi.json", MAX_FACT_CONTRACTS);
    push_fact_path(contracts, root, "openapi.yaml", MAX_FACT_CONTRACTS);
    push_fact_path(contracts, root, "openapi.yml", MAX_FACT_CONTRACTS);
    push_fact_path(contracts, root, "proto/command.proto", MAX_FACT_CONTRACTS);
}
