use super::fs_scan::list_immediate_subdirs;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

pub(super) const DEFAULT_MEMORY_FILE_CANDIDATES: &[&str] = &[
    "AGENTS.md",
    "AGENTS.context",
    "README.md",
    "README.context",
    "docs/README.md",
    "docs/README.context",
    "docs/QUICK_START.md",
    "docs/QUICK_START.context",
    "ARCHITECTURE.md",
    "ARCHITECTURE.context",
    "docs/ARCHITECTURE.md",
    "docs/ARCHITECTURE.context",
    "GOALS.md",
    "GOALS.context",
    "docs/GOALS.md",
    "docs/GOALS.context",
    "PHILOSOPHY.md",
    "PHILOSOPHY.context",
    "CONTRIBUTING.md",
    "DEVELOPMENT.md",
    "docs/DEVELOPMENT.md",
    "tests/README.md",
    ".env.example",
    ".env.sample",
    ".env.template",
    ".env.dist",
    ".nvmrc",
    ".node-version",
    ".python-version",
    ".ruby-version",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "requirements.txt",
    "Makefile",
    "Justfile",
    "justfile",
    "rust-toolchain.toml",
    "rust-toolchain",
    "tsconfig.json",
    "Dockerfile",
    "docker-compose.yml",
    ".gitlab-ci.yml",
    ".editorconfig",
    ".tool-versions",
    ".devcontainer/devcontainer.json",
    ".vscode/tasks.json",
    ".vscode/launch.json",
    ".vscode/settings.json",
    ".vscode/extensions.json",
];

const MODULE_MEMORY_FILE_CANDIDATES: &[&str] = &[
    // Docs first (these drive the "how do I run/test/deploy" UX).
    "AGENTS.md",
    "AGENTS.context",
    "README.md",
    "README.context",
    "docs/README.md",
    "docs/README.context",
    "docs/QUICK_START.md",
    "docs/QUICK_START.context",
    "ARCHITECTURE.md",
    "ARCHITECTURE.context",
    "docs/ARCHITECTURE.md",
    "docs/ARCHITECTURE.context",
    "GOALS.md",
    "GOALS.context",
    "docs/GOALS.md",
    "docs/GOALS.context",
    "PHILOSOPHY.md",
    "PHILOSOPHY.context",
    "CONTRIBUTING.md",
    "DEVELOPMENT.md",
    "docs/DEVELOPMENT.md",
    "tests/README.md",
    // Config hints (bounded, high-signal).
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "requirements.txt",
    "Makefile",
    "Justfile",
    "justfile",
    "rust-toolchain.toml",
    "rust-toolchain",
    "tsconfig.json",
    "Dockerfile",
    "docker-compose.yml",
    ".gitlab-ci.yml",
    ".editorconfig",
];

pub(super) fn is_disallowed_memory_file(candidate: &str) -> bool {
    let rel = candidate.trim().replace('\\', "/");
    if rel == ".agents" || rel.starts_with(".agents/") {
        return true;
    }
    crate::tools::secrets::is_potential_secret_path(&rel)
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

pub(super) fn collect_github_workflow_candidates(
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

fn doc_candidate_score(rel: &str) -> i32 {
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

fn collect_fallback_doc_candidates(root: &Path, seen: &mut HashSet<String>) -> Vec<String> {
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

fn is_doc_memory_candidate(rel: &str) -> bool {
    Path::new(rel)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "md" | "mdx" | "rst" | "adoc" | "txt" | "context"
            )
        })
}

pub(super) fn config_candidate_score(rel: &str) -> i32 {
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

pub(super) fn collect_memory_file_candidates(root: &Path) -> Vec<String> {
    // Memory-pack candidate ordering is a UX contract:
    // - start with docs (AGENTS/README/quick start), because they usually contain "how to run/test"
    // - interleave in a few build/config hints early (Cargo.toml/package.json/workflows)
    // - keep it deterministic and stable across calls (so cursor pagination is predictable)
    let mut seen = HashSet::new();
    let mut docs: Vec<(usize, String)> = Vec::new();
    let mut configs: Vec<(usize, String)> = Vec::new();

    for (idx, &candidate) in DEFAULT_MEMORY_FILE_CANDIDATES.iter().enumerate() {
        let rel = candidate.trim().replace('\\', "/");
        if rel.is_empty() || rel == "." {
            continue;
        }
        if is_disallowed_memory_file(&rel) {
            continue;
        }
        if !root.join(&rel).is_file() {
            continue;
        }
        if !seen.insert(rel.clone()) {
            continue;
        }

        if is_doc_memory_candidate(&rel) {
            docs.push((idx, rel));
        } else {
            configs.push((idx, rel));
        }
    }

    // If a repo is nested under a wrapper directory (common in multi-repo workspaces), pull a small,
    // deterministic allowlist of memory candidates from immediate subdirectories as well.
    //
    // This keeps "project memory" useful even when the root itself is mostly empty.
    let base_idx = DEFAULT_MEMORY_FILE_CANDIDATES.len();
    for (dir_idx, dir_name) in list_immediate_subdirs(root, 24).into_iter().enumerate() {
        let dir_rel = dir_name.trim().replace('\\', "/");
        if dir_rel.is_empty() || dir_rel == "." {
            continue;
        }
        if is_disallowed_memory_file(&dir_rel) {
            continue;
        }
        for (inner_idx, &candidate) in MODULE_MEMORY_FILE_CANDIDATES.iter().enumerate() {
            let candidate = candidate.trim().replace('\\', "/");
            if candidate.is_empty() || candidate == "." {
                continue;
            }
            let rel = format!("{dir_rel}/{candidate}");
            if is_disallowed_memory_file(&rel) {
                continue;
            }
            if !root.join(&rel).is_file() {
                continue;
            }
            if !seen.insert(rel.clone()) {
                continue;
            }
            let idx = base_idx
                .saturating_add(dir_idx.saturating_mul(MODULE_MEMORY_FILE_CANDIDATES.len()))
                .saturating_add(inner_idx);
            if is_doc_memory_candidate(&rel) {
                docs.push((idx, rel));
            } else {
                configs.push((idx, rel));
            }
        }
    }

    // Depth-2 wrapper fallback (bounded): if the root is a thin wrapper with no candidates at the
    // root or depth-1, scan one more level down. This covers common layouts like `X/foo/1/*`.
    if docs.is_empty() && configs.is_empty() {
        let base_idx2 =
            base_idx.saturating_add(24usize.saturating_mul(MODULE_MEMORY_FILE_CANDIDATES.len()));
        for (outer_idx, outer_name) in list_immediate_subdirs(root, 8).into_iter().enumerate() {
            let outer_rel = outer_name.trim().replace('\\', "/");
            if outer_rel.is_empty() || outer_rel == "." {
                continue;
            }
            let outer_root = root.join(&outer_rel);
            if !outer_root.is_dir() {
                continue;
            }
            for (inner_idx, inner_name) in list_immediate_subdirs(&outer_root, 8)
                .into_iter()
                .enumerate()
            {
                let inner_rel = inner_name.trim().replace('\\', "/");
                if inner_rel.is_empty() || inner_rel == "." {
                    continue;
                }
                let module_prefix = format!("{outer_rel}/{inner_rel}");
                if is_disallowed_memory_file(&module_prefix) {
                    continue;
                }
                for (candidate_idx, &candidate) in MODULE_MEMORY_FILE_CANDIDATES.iter().enumerate()
                {
                    let candidate = candidate.trim().replace('\\', "/");
                    if candidate.is_empty() || candidate == "." {
                        continue;
                    }
                    let rel = format!("{module_prefix}/{candidate}");
                    if is_disallowed_memory_file(&rel) {
                        continue;
                    }
                    if !root.join(&rel).is_file() {
                        continue;
                    }
                    if !seen.insert(rel.clone()) {
                        continue;
                    }
                    let idx = base_idx2
                        .saturating_add(outer_idx.saturating_mul(10_000))
                        .saturating_add(
                            inner_idx.saturating_mul(MODULE_MEMORY_FILE_CANDIDATES.len()),
                        )
                        .saturating_add(candidate_idx);
                    if is_doc_memory_candidate(&rel) {
                        docs.push((idx, rel));
                    } else {
                        configs.push((idx, rel));
                    }
                }
            }
        }
    }

    // Workflows are high-signal config for agents; keep a couple and treat them like configs.
    for rel in collect_github_workflow_candidates(root, &mut seen) {
        if !root.join(&rel).is_file() {
            continue;
        }
        configs.push((usize::MAX, rel));
    }

    // Fallback: if the allowlist produced no docs, discover a few doc-like files from common
    // doc roots. This keeps memory packs useful in repos that don't follow README/AGENTS naming.
    if docs.is_empty() {
        let base_idx3 = usize::MAX.saturating_sub(10_000);
        for (idx, rel) in collect_fallback_doc_candidates(root, &mut seen)
            .into_iter()
            .enumerate()
        {
            docs.push((base_idx3.saturating_add(idx), rel));
        }
    }

    // Preserve doc order, but prioritize high-value configs deterministically.
    configs.sort_by(|(a_idx, a_rel), (b_idx, b_rel)| {
        let a_score = config_candidate_score(a_rel);
        let b_score = config_candidate_score(b_rel);
        b_score
            .cmp(&a_score)
            .then_with(|| a_idx.cmp(b_idx))
            .then_with(|| a_rel.cmp(b_rel))
    });

    let mut out = Vec::new();
    let mut doc_idx = 0usize;
    let mut cfg_idx = 0usize;

    // Keep the first couple of docs uninterrupted (AGENTS + README), then weave in configs.
    for _ in 0..2 {
        if doc_idx < docs.len() {
            out.push(docs[doc_idx].1.clone());
            doc_idx += 1;
        }
    }

    while doc_idx < docs.len() || cfg_idx < configs.len() {
        if cfg_idx < configs.len() {
            out.push(configs[cfg_idx].1.clone());
            cfg_idx += 1;
        }
        if doc_idx < docs.len() {
            out.push(docs[doc_idx].1.clone());
            doc_idx += 1;
        }
    }

    out
}

pub(super) fn ops_candidate_score(rel: &str) -> i32 {
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

pub(super) fn collect_ops_file_candidates(root: &Path) -> Vec<String> {
    let mut candidates = collect_memory_file_candidates(root);
    candidates.sort_by(|a, b| {
        ops_candidate_score(b)
            .cmp(&ops_candidate_score(a))
            .then_with(|| a.cmp(b))
    });
    candidates
}
