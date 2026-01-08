use std::path::{Path, PathBuf};

pub const CONTEXT_DIR_NAME: &str = ".context";
pub const LEGACY_CONTEXT_DIR_NAME: &str = ".context-finder";

pub const CONTEXT_CACHE_DIR_NAME: &str = "context";
pub const LEGACY_CONTEXT_CACHE_DIR_NAME: &str = "context-finder";

#[must_use]
pub fn context_dir_for_project_root(root: &Path) -> PathBuf {
    let preferred = root.join(CONTEXT_DIR_NAME);
    if preferred.exists() {
        return preferred;
    }
    let legacy = root.join(LEGACY_CONTEXT_DIR_NAME);
    if legacy.exists() {
        return legacy;
    }
    preferred
}

#[must_use]
pub fn find_context_dir_from_path(path: &Path) -> Option<PathBuf> {
    let mut current = path.parent();
    while let Some(dir) = current {
        if let Some(name) = dir.file_name().and_then(|s| s.to_str()) {
            if name == CONTEXT_DIR_NAME || name == LEGACY_CONTEXT_DIR_NAME {
                return Some(dir.to_path_buf());
            }
        }
        current = dir.parent();
    }
    None
}

#[must_use]
pub fn is_context_dir_name(name: &str) -> bool {
    name == CONTEXT_DIR_NAME || name == LEGACY_CONTEXT_DIR_NAME
}
