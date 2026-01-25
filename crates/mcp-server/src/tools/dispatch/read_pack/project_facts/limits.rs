use super::super::candidates::is_disallowed_memory_file;
use std::path::Path;

pub(super) const MAX_FACT_ECOSYSTEMS: usize = 8;
pub(super) const MAX_FACT_BUILD_TOOLS: usize = 10;
pub(super) const MAX_FACT_CI: usize = 6;
pub(super) const MAX_FACT_CONTRACTS: usize = 8;
pub(super) const MAX_FACT_KEY_DIRS: usize = 12;
pub(super) const MAX_FACT_MODULES: usize = 16;
pub(super) const MAX_FACT_ENTRY_POINTS: usize = 10;
pub(super) const MAX_FACT_KEY_CONFIGS: usize = 20;

pub(super) fn push_fact(out: &mut Vec<String>, value: &str, max: usize) {
    if out.len() >= max {
        return;
    }
    if out.iter().any(|existing| existing == value) {
        return;
    }
    out.push(value.to_string());
}

pub(super) fn push_fact_path(out: &mut Vec<String>, root: &Path, rel: &str, max: usize) {
    if out.len() >= max {
        return;
    }
    if is_disallowed_memory_file(rel) {
        return;
    }
    if !root.join(rel).is_file() {
        return;
    }
    push_fact(out, rel, max);
}

pub(super) fn push_fact_dir(out: &mut Vec<String>, root: &Path, rel: &str, max: usize) {
    if out.len() >= max {
        return;
    }
    if !root.join(rel).is_dir() {
        return;
    }
    push_fact(out, rel, max);
}
