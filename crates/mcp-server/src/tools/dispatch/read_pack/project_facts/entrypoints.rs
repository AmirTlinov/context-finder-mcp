use super::limits::{push_fact_path, MAX_FACT_ENTRY_POINTS};
use std::fs;
use std::path::Path;

pub(super) fn append_entrypoints(root: &Path, entry_points: &mut Vec<String>) {
    // Entrypoint candidates.
    push_fact_path(entry_points, root, "src/main.rs", MAX_FACT_ENTRY_POINTS);
    push_fact_path(entry_points, root, "src/lib.rs", MAX_FACT_ENTRY_POINTS);
    push_fact_path(entry_points, root, "main.go", MAX_FACT_ENTRY_POINTS);
    push_fact_path(entry_points, root, "main.py", MAX_FACT_ENTRY_POINTS);
    push_fact_path(entry_points, root, "app.py", MAX_FACT_ENTRY_POINTS);
    push_fact_path(entry_points, root, "src/main.py", MAX_FACT_ENTRY_POINTS);
    push_fact_path(entry_points, root, "src/app.py", MAX_FACT_ENTRY_POINTS);
    push_fact_path(entry_points, root, "src/index.ts", MAX_FACT_ENTRY_POINTS);
    push_fact_path(entry_points, root, "src/index.js", MAX_FACT_ENTRY_POINTS);
    push_fact_path(entry_points, root, "index.ts", MAX_FACT_ENTRY_POINTS);
    push_fact_path(entry_points, root, "index.js", MAX_FACT_ENTRY_POINTS);
    push_fact_path(entry_points, root, "src/server.ts", MAX_FACT_ENTRY_POINTS);
    push_fact_path(entry_points, root, "src/server.js", MAX_FACT_ENTRY_POINTS);
    push_fact_path(entry_points, root, "src/main.ts", MAX_FACT_ENTRY_POINTS);
    push_fact_path(entry_points, root, "src/main.js", MAX_FACT_ENTRY_POINTS);
    push_fact_path(entry_points, root, "cmd/main.go", MAX_FACT_ENTRY_POINTS);
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
                push_fact_path(entry_points, root, &rel, MAX_FACT_ENTRY_POINTS);
            }
        }
    }
}
