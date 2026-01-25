use super::{is_disallowed_memory_file, is_doc_memory_candidate};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

pub(in crate::tools::dispatch::read_pack) fn collect_github_workflow_candidates(
    root: &Path,
    seen: &mut HashSet<String>,
) -> Vec<String> {
    const MAX_WORKFLOWS: usize = 2;
    let workflows_dir = root.join(".github").join("workflows");
    let Ok(entries) = fs::read_dir(&workflows_dir) else {
        return Vec::new();
    };

    let mut workflows: Vec<String> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let ty = entry.file_type().ok()?;
            if !ty.is_file() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let lowered = name.to_lowercase();
            if !(lowered.ends_with(".yml") || lowered.ends_with(".yaml")) {
                return None;
            }
            Some(format!(".github/workflows/{name}"))
        })
        .collect();

    workflows.sort();
    workflows.truncate(MAX_WORKFLOWS);

    let mut out = Vec::new();
    for candidate in workflows {
        push_memory_candidate(&mut out, seen, &candidate);
    }
    out
}

pub(super) fn doc_candidate_score(rel: &str) -> i32 {
    let normalized = rel.replace('\\', "/").to_ascii_lowercase();
    let file_name = normalized.rsplit('/').next().unwrap_or(normalized.as_str());

    match file_name {
        name if name.starts_with("agents.") => 220,
        "agents.md" | "readme.md" => 210,
        name if name.starts_with("readme.") => 205,
        "docs/quick_start.md" | "quick_start.md" => 200,
        "getting_started.md" | "get_started.md" => 198,
        "install.md" | "setup.md" | "usage.md" | "guide.md" => 196,
        "development.md" | "contributing.md" | "hacking.md" => 194,
        "architecture.md" | "design.md" | "overview.md" => 192,
        "philosophy.md" | "goals.md" => 190,
        "security.md" => 180,
        _ => 100,
    }
}

pub(super) fn collect_fallback_doc_candidates(
    root: &Path,
    seen: &mut HashSet<String>,
) -> Vec<String> {
    const MAX_DIR_ENTRIES: usize = 512;
    const MAX_DOCS: usize = 12;
    const DOC_DIRS: &[&str] = &["", "docs", "doc", "Documentation"];

    let mut candidates: Vec<(i32, String)> = Vec::new();

    for dir_rel in DOC_DIRS {
        let dir_path = if dir_rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(dir_rel)
        };
        if !dir_path.is_dir() {
            continue;
        }

        let Ok(entries) = fs::read_dir(&dir_path) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let ty = entry.file_type().ok()?;
                if !ty.is_file() {
                    return None;
                }
                Some(entry.file_name().to_string_lossy().to_string())
            })
            .take(MAX_DIR_ENTRIES)
            .collect();
        names.sort();

        for name in names {
            if name.trim().is_empty() {
                continue;
            }
            let rel = if dir_rel.is_empty() {
                name.clone()
            } else {
                format!("{dir_rel}/{name}")
            };
            let rel_norm = rel.replace('\\', "/");
            if is_disallowed_memory_file(&rel_norm) {
                continue;
            }
            if !is_doc_memory_candidate(&rel_norm) {
                continue;
            }
            if !root.join(&rel_norm).is_file() {
                continue;
            }
            if !seen.insert(rel_norm.clone()) {
                continue;
            }
            let score = doc_candidate_score(&rel_norm);
            candidates.push((score, rel_norm));
        }
    }

    candidates.sort_by(|(a_score, a_rel), (b_score, b_rel)| {
        b_score.cmp(a_score).then_with(|| a_rel.cmp(b_rel))
    });
    candidates.truncate(MAX_DOCS);
    candidates.into_iter().map(|(_, rel)| rel).collect()
}

fn push_memory_candidate(out: &mut Vec<String>, seen: &mut HashSet<String>, candidate: &str) {
    let rel = candidate.trim().replace('\\', "/");
    if rel.is_empty() || rel == "." {
        return;
    }
    if is_disallowed_memory_file(&rel) {
        return;
    }
    if seen.insert(rel.clone()) {
        out.push(rel);
    }
}
