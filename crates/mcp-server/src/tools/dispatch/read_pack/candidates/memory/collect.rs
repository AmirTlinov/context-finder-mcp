use super::super::doc::{collect_fallback_doc_candidates, collect_github_workflow_candidates};
use super::super::{
    is_disallowed_memory_file, is_doc_memory_candidate, DEFAULT_MEMORY_FILE_CANDIDATES,
    MODULE_MEMORY_FILE_CANDIDATES,
};
use super::score::config_candidate_score;
use crate::tools::dispatch::read_pack::fs_scan::list_immediate_subdirs;
use std::collections::HashSet;
use std::path::Path;

pub(in crate::tools::dispatch::read_pack) fn collect_memory_file_candidates(
    root: &Path,
) -> Vec<String> {
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
