use super::super::fs_scan::list_immediate_subdirs;
use super::limits::{push_fact_dir, MAX_FACT_KEY_DIRS};
use std::path::Path;

pub(super) fn collect_key_dirs(root: &Path, key_dirs: &mut Vec<String>) {
    for rel in list_immediate_subdirs(root, MAX_FACT_KEY_DIRS) {
        push_fact_dir(key_dirs, root, &rel, MAX_FACT_KEY_DIRS);
    }
}
