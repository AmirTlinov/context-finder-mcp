mod resolve;
mod session_defaults;

pub(super) use resolve::{
    canonicalize_root, canonicalize_root_path, collect_relative_hints, env_root_override,
    find_project_root, hint_score_for_root, rel_path_string, resolve_root_from_absolute_hints,
    root_path_from_mcp_uri,
};
pub(super) use session_defaults::{trimmed_non_empty, SessionDefaults};
