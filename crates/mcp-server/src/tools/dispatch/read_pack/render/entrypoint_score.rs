pub(in crate::tools::dispatch::read_pack) fn entrypoint_candidate_score(rel: &str) -> i32 {
    let normalized = rel.replace('\\', "/").to_ascii_lowercase();
    match normalized.as_str() {
        "src/main.rs" => 300,
        "src/lib.rs" => 260,
        "main.go" | "main.py" | "app.py" => 250,
        "src/main.py" | "src/app.py" => 240,
        "src/index.ts" | "src/index.js" => 230,
        "src/main.ts" | "src/main.js" => 225,
        "cargo.toml" | "package.json" | "pyproject.toml" | "go.mod" => 200,
        _ if normalized.ends_with("/src/main.rs") => 190,
        _ if normalized.ends_with("/src/lib.rs") => 170,
        _ if normalized.ends_with("/src/index.ts")
            || normalized.ends_with("/src/index.js")
            || normalized.ends_with("/src/main.ts")
            || normalized.ends_with("/src/main.js") =>
        {
            165
        }
        _ if normalized.ends_with("/main.go")
            || normalized.ends_with("/main.py")
            || normalized.ends_with("/app.py") =>
        {
            160
        }
        _ if normalized.contains("xtask") && normalized.ends_with("/src/main.rs") => 210,
        _ => 10,
    }
}
