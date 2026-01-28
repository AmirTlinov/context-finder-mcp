use super::super::super::{ContextFinderService, ContextPackRequest, EngineLock};
use context_indexer::AnchorPolicy;
use std::path::Path;

pub(super) fn select_language(
    raw: Option<&str>,
    engine: &mut EngineLock,
) -> context_graph::GraphLanguage {
    raw.map_or_else(
        || {
            let chunks = engine.engine_mut().context_search.hybrid().chunks();
            ContextFinderService::detect_language(chunks)
        },
        |lang| ContextFinderService::parse_language(Some(lang)),
    )
}

pub(super) fn effective_anchor_policy(requested: AnchorPolicy) -> AnchorPolicy {
    let override_raw = std::env::var("CONTEXT_ANCHOR_POLICY").ok();
    match override_raw
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "" => requested,
        "auto" => AnchorPolicy::Auto,
        "off" => AnchorPolicy::Off,
        _ => requested,
    }
}

pub(super) fn disambiguate_context_pack_path_as_scope_hint_if_root_set(
    session_root: Option<&Path>,
    request: &mut ContextPackRequest,
) -> bool {
    let has_explicit_filters = request
        .include_paths
        .as_ref()
        .is_some_and(|v| !v.is_empty())
        || request
            .exclude_paths
            .as_ref()
            .is_some_and(|v| !v.is_empty())
        || request
            .file_pattern
            .as_deref()
            .map(str::trim)
            .is_some_and(|v| !v.is_empty());
    if has_explicit_filters {
        return false;
    }

    let Some(session_root) = session_root else {
        return false;
    };
    let Some(raw_path) = request.path.as_deref() else {
        return false;
    };

    let raw_path = raw_path.trim();
    if raw_path.is_empty() {
        return false;
    }
    if Path::new(raw_path).is_absolute() {
        return false;
    }

    let normalized = raw_path.replace('\\', "/");
    if normalized.contains('*') || normalized.contains('?') {
        request.file_pattern = Some(normalized);
        request.path = None;
        return true;
    }

    let candidate = session_root.join(&normalized);
    let is_dir = std::fs::metadata(&candidate)
        .ok()
        .map(|meta| meta.is_dir())
        .unwrap_or(false);
    if is_dir {
        request.include_paths = Some(vec![normalized]);
    } else {
        request.file_pattern = Some(normalized);
    }
    request.path = None;
    true
}
