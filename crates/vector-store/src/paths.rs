use std::path::{Path, PathBuf};

pub const CONTEXT_DIR_NAME: &str = ".context";

pub const AGENTS_DIR_NAME: &str = ".agents";
pub const AGENTS_MCP_DIR_NAME: &str = "mcp";

pub const CONTEXT_CACHE_DIR_NAME: &str = "context";

#[must_use]
pub fn default_context_dir_rel() -> PathBuf {
    PathBuf::from(AGENTS_DIR_NAME)
        .join(AGENTS_MCP_DIR_NAME)
        .join(CONTEXT_DIR_NAME)
}

#[must_use]
pub fn context_dir_for_project_root(root: &Path) -> PathBuf {
    root.join(default_context_dir_rel())
}

#[must_use]
pub fn find_context_dir_from_path(path: &Path) -> Option<PathBuf> {
    let mut current = path.parent();
    while let Some(dir) = current {
        if let Some(name) = dir.file_name().and_then(|s| s.to_str()) {
            if name == CONTEXT_DIR_NAME {
                return Some(dir.to_path_buf());
            }
        }
        current = dir.parent();
    }
    None
}

#[must_use]
pub fn is_context_dir_name(name: &str) -> bool {
    name == CONTEXT_DIR_NAME
}
