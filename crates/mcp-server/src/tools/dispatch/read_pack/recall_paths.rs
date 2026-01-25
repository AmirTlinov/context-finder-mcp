use std::collections::HashSet;
use std::path::Path;

pub(super) fn recall_prefix_matches(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim().replace('\\', "/");
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }

    path == prefix || path.starts_with(&format!("{prefix}/"))
}

pub(super) fn recall_path_allowed(
    path: &str,
    include_paths: &[String],
    exclude_paths: &[String],
) -> bool {
    let path = path.replace('\\', "/");
    if exclude_paths
        .iter()
        .any(|prefix| !prefix.trim().is_empty() && recall_prefix_matches(&path, prefix))
    {
        return false;
    }

    if include_paths.is_empty() {
        return true;
    }

    include_paths
        .iter()
        .any(|prefix| !prefix.trim().is_empty() && recall_prefix_matches(&path, prefix))
}

pub(super) fn scan_file_pattern_for_include_prefix(root: &Path, prefix: &str) -> Option<String> {
    let normalized = prefix.trim().replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    if normalized.is_empty() {
        return None;
    }

    if root.join(normalized).is_dir() {
        return Some(format!("{normalized}/"));
    }

    Some(normalized.to_string())
}

pub(super) fn merge_recall_prefix_lists(
    base: &[String],
    extra: &[String],
    max: usize,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for value in base.iter().chain(extra.iter()) {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if out.len() >= max {
            break;
        }
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    }

    out
}
