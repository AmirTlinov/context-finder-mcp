use super::super::fs_scan::list_immediate_subdirs;
use super::limits::{push_fact, MAX_FACT_ENTRY_POINTS, MAX_FACT_MODULES};
use super::markers::scan_dir_markers_for_facts;
use std::fs;
use std::path::Path;

pub(super) fn scan_topology(
    root: &Path,
    ecosystems: &mut Vec<String>,
    build_tools: &mut Vec<String>,
    modules: &mut Vec<String>,
    entry_points: &mut Vec<String>,
    key_configs: &mut Vec<String>,
) {
    // Topology scan (bounded): if the project root is a wrapper (monorepo container, nested repo),
    // detect marker files in immediate subdirectories and surface them as modules + facts.
    for name in list_immediate_subdirs(root, 24) {
        if modules.len() >= MAX_FACT_MODULES {
            break;
        }
        scan_dir_markers_for_facts(
            ecosystems,
            build_tools,
            modules,
            entry_points,
            key_configs,
            root,
            &name,
        );
    }

    scan_container_modules(
        root,
        ecosystems,
        build_tools,
        modules,
        entry_points,
        key_configs,
    );

    scan_wrapper_fallback(
        root,
        ecosystems,
        build_tools,
        modules,
        entry_points,
        key_configs,
    );

    scan_cmd_modules(root, modules);
}

fn scan_container_modules(
    root: &Path,
    ecosystems: &mut Vec<String>,
    build_tools: &mut Vec<String>,
    modules: &mut Vec<String>,
    entry_points: &mut Vec<String>,
    key_configs: &mut Vec<String>,
) {
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
                ecosystems,
                build_tools,
                modules,
                entry_points,
                key_configs,
                root,
                &rel,
            );
        }
    }
}

fn scan_wrapper_fallback(
    root: &Path,
    ecosystems: &mut Vec<String>,
    build_tools: &mut Vec<String>,
    modules: &mut Vec<String>,
    entry_points: &mut Vec<String>,
    key_configs: &mut Vec<String>,
) {
    // Wrapper → nested repo fallback (depth-2, bounded).
    let looks_like_wrapper = ecosystems.is_empty() && modules.is_empty() && entry_points.is_empty();
    if !looks_like_wrapper {
        return;
    }

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
                ecosystems,
                build_tools,
                modules,
                entry_points,
                key_configs,
                root,
                &rel,
            );
        }
    }
}

fn scan_cmd_modules(root: &Path, modules: &mut Vec<String>) {
    // Go-style cmd/* entrypoints as modules.
    if modules.len() >= MAX_FACT_MODULES || !root.join("cmd").is_dir() {
        return;
    }
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
            push_fact(modules, &rel, MAX_FACT_MODULES);
            if modules.len() >= MAX_FACT_MODULES {
                break;
            }
        }
    }
}
