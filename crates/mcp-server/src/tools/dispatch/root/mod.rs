mod resolve;
mod service;
mod session_defaults;

pub(super) use resolve::{
    canonicalize_root, canonicalize_root_path, collect_relative_hints, env_root_override,
    hint_score_for_root, rel_path_string, resolve_root_from_absolute_hints, root_path_from_mcp_uri,
    scope_hint_from_relative_path,
};
pub(super) use service::workspace_roots_preview;
pub(super) use session_defaults::{trimmed_non_empty, SessionDefaults};
