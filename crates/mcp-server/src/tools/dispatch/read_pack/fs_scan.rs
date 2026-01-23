use std::fs;
use std::path::Path;

fn top_level_dir_priority(name: &str) -> i32 {
    // Deterministic, repo-agnostic topology hints.
    //
    // These names are common in real-world repos. We use them only to order a bounded scan of
    // *existing* directories. This is not project-specific behavior.
    match name.to_ascii_lowercase().as_str() {
        "src" => 300,
        "crates" => 290,
        "backend" => 280,
        "frontend" => 275,
        "server" => 270,
        "client" => 265,
        "api" => 260,
        "web" => 255,
        "app" => 250,
        "apps" => 248,
        "services" => 246,
        "packages" => 244,
        "libs" => 242,
        "lib" => 240,
        "config" => 232,
        "configs" => 231,
        "docs" => 230,
        "scripts" => 220,
        "tests" => 210,
        "cmd" => 205,
        "contracts" => 204,
        "proto" => 203,
        ".github" => 200,
        "ai" => 198,
        "agents" => 196,
        "tools" => 192,
        "examples" => 186,
        "connectors" => 184,
        "infra" => 180,
        "deploy" => 175,
        "ops" => 170,
        _ => 0,
    }
}

pub(super) fn list_immediate_subdirs(root: &Path, max: usize) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };

    let mut out: Vec<String> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let ty = entry.file_type().ok()?;
            if !ty.is_dir() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.is_empty() {
                return None;
            }
            // Skip obvious noise.
            if matches!(
                name.as_str(),
                "target"
                    | "node_modules"
                    | ".agents"
                    | ".context"
                    | ".context-finder"
                    | ".git"
                    | ".hg"
                    | ".svn"
                    | ".idea"
                    | ".pytest_cache"
                    | ".mypy_cache"
                    | ".ruff_cache"
                    | ".venv"
                    | "venv"
                    | "__pycache__"
            ) {
                return None;
            }
            Some(name)
        })
        .collect();

    out.sort_by(|a, b| {
        top_level_dir_priority(b)
            .cmp(&top_level_dir_priority(a))
            .then_with(|| a.cmp(b))
    });
    out.truncate(max);
    out
}
