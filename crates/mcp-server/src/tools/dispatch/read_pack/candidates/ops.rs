use super::collect_memory_file_candidates;
use std::path::Path;

pub(in crate::tools::dispatch::read_pack) fn ops_candidate_score(rel: &str) -> i32 {
    // Rank candidates by how likely they contain *actionable* commands.
    // This keeps ops-recall deterministic and avoids falling into domain docs.
    let normalized = rel.replace('\\', "/").to_ascii_lowercase();

    match normalized.as_str() {
        "agents.md" => 200,
        "readme.md" => 195,
        "docs/quick_start.md" => 190,
        "development.md" => 188,
        "docs/development.md" => 186,
        "tests/readme.md" => 184,
        "contributing.md" => 180,
        "makefile" | "justfile" | "dockerfile" | "docker-compose.yml" => 170,
        "cargo.toml" | "package.json" | "pyproject.toml" | "go.mod" => 160,
        _ if normalized.starts_with(".github/workflows/") => 175,
        _ => {
            if normalized.ends_with("/agents.md") {
                190
            } else if normalized.ends_with("/readme.md") {
                185
            } else if normalized.ends_with("/docs/quick_start.md") {
                180
            } else if normalized.ends_with("/development.md")
                || normalized.ends_with("/contributing.md")
            {
                175
            } else if normalized.ends_with("/makefile")
                || normalized.ends_with("/justfile")
                || normalized.ends_with("/dockerfile")
                || normalized.ends_with("/docker-compose.yml")
            {
                160
            } else if normalized.ends_with("/cargo.toml")
                || normalized.ends_with("/package.json")
                || normalized.ends_with("/pyproject.toml")
                || normalized.ends_with("/go.mod")
            {
                150
            } else if normalized.ends_with(".md") {
                60
            } else {
                20
            }
        }
    }
}

pub(in crate::tools::dispatch::read_pack) fn collect_ops_file_candidates(
    root: &Path,
) -> Vec<String> {
    let mut candidates = collect_memory_file_candidates(root);
    candidates.sort_by(|a, b| {
        ops_candidate_score(b)
            .cmp(&ops_candidate_score(a))
            .then_with(|| a.cmp(b))
    });
    candidates
}
