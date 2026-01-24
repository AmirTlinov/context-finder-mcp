use std::path::{Path, PathBuf};

pub const CONTEXT_DIR_NAME: &str = ".context";
pub const LEGACY_CONTEXT_DIR_NAME: &str = ".context-finder";

pub const AGENTS_DIR_NAME: &str = ".agents";
pub const AGENTS_MCP_DIR_NAME: &str = "mcp";
// Legacy layout (pre-2026): `.agents/mcp/context/.context/…`
pub const AGENTS_MCP_CONTEXT_DIR_NAME: &str = "context";

pub const CONTEXT_CACHE_DIR_NAME: &str = "context";
pub const LEGACY_CONTEXT_CACHE_DIR_NAME: &str = "context-finder";

#[must_use]
pub fn default_context_dir_rel() -> PathBuf {
    PathBuf::from(AGENTS_DIR_NAME)
        .join(AGENTS_MCP_DIR_NAME)
        .join(CONTEXT_DIR_NAME)
}

#[must_use]
pub fn context_dir_for_project_root(root: &Path) -> PathBuf {
    let preferred = root.join(default_context_dir_rel());
    if preferred.exists() {
        return preferred;
    }

    // Best-effort migration: old layout used `.agents/mcp/context/.context`.
    let legacy_agents = root
        .join(AGENTS_DIR_NAME)
        .join(AGENTS_MCP_DIR_NAME)
        .join(AGENTS_MCP_CONTEXT_DIR_NAME)
        .join(CONTEXT_DIR_NAME);
    if legacy_agents.is_dir() {
        if let Some(parent) = preferred.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::rename(&legacy_agents, &preferred);
        if preferred.exists() {
            return preferred;
        }
    }

    // Backward compatibility: older layouts stored project-scoped state directly under the repo
    // root. Keep honoring them when present to avoid surprise migrations or duplicate caches.
    let root_level = root.join(CONTEXT_DIR_NAME);
    if root_level.exists() {
        return root_level;
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
