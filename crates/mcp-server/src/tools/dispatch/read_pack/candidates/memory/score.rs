pub(in crate::tools::dispatch::read_pack) fn config_candidate_score(rel: &str) -> i32 {
    let normalized = rel.replace('\\', "/").to_ascii_lowercase();
    match normalized.as_str() {
        "cargo.toml" | "package.json" | "pyproject.toml" | "go.mod" => 100,
        _ if normalized.ends_with("/cargo.toml")
            || normalized.ends_with("/package.json")
            || normalized.ends_with("/pyproject.toml")
            || normalized.ends_with("/go.mod") =>
        {
            95
        }
        "requirements.txt" | "makefile" | "justfile" | "dockerfile" | "docker-compose.yml" => 90,
        _ if normalized.ends_with("/requirements.txt")
            || normalized.ends_with("/makefile")
            || normalized.ends_with("/justfile")
            || normalized.ends_with("/dockerfile")
            || normalized.ends_with("/docker-compose.yml") =>
        {
            85
        }
        "rust-toolchain.toml" | "rust-toolchain" | "tsconfig.json" => 75,
        _ if normalized.ends_with("/rust-toolchain.toml")
            || normalized.ends_with("/rust-toolchain")
            || normalized.ends_with("/tsconfig.json") =>
        {
            70
        }
        ".gitlab-ci.yml" => 70,
        _ if normalized.ends_with("/.gitlab-ci.yml") => 65,
        ".vscode/tasks.json" => 55,
        ".vscode/launch.json" => 45,
        ".vscode/settings.json" => 35,
        ".vscode/extensions.json" => 25,
        ".editorconfig" | ".tool-versions" | ".devcontainer/devcontainer.json" => 40,
        _ if normalized.ends_with("/.editorconfig")
            || normalized.ends_with("/.tool-versions")
            || normalized.ends_with("/.devcontainer/devcontainer.json") =>
        {
            35
        }
        ".env.example" | ".env.sample" | ".env.template" | ".env.dist" => 20,
        _ if normalized.ends_with("/.env.example")
            || normalized.ends_with("/.env.sample")
            || normalized.ends_with("/.env.template")
            || normalized.ends_with("/.env.dist") =>
        {
            15
        }
        _ if normalized.starts_with(".github/workflows/") => 85,
        _ => 10,
    }
}
