use super::super::cursors::trimmed_non_empty_str;
use super::super::{ContextFinderService, ReadPackRequest};
use crate::tools::dispatch::root::rel_path_string;
use std::path::Path;

pub(super) async fn apply_path_hints(
    service: &ContextFinderService,
    request: &mut ReadPackRequest,
) {
    let cursor_missing = trimmed_non_empty_str(request.cursor.as_deref()).is_none();
    let file_missing = trimmed_non_empty_str(request.file.as_deref()).is_none();
    let file_pattern_missing = trimmed_non_empty_str(request.file_pattern.as_deref()).is_none();
    if !cursor_missing || !file_missing || !file_pattern_missing {
        return;
    }

    let Some(raw_path) = trimmed_non_empty_str(request.path.as_deref()) else {
        return;
    };
    let session_root = { service.session.lock().await.clone_root().map(|(r, _)| r) };
    let Some(session_root) = session_root.as_ref() else {
        return;
    };

    let raw = Path::new(raw_path);
    if raw.is_absolute() {
        if let Ok(canonical) = raw.canonicalize() {
            if canonical.starts_with(session_root) {
                if let Ok(rel) = canonical.strip_prefix(session_root) {
                    if let Some(rel) = rel_path_string(rel) {
                        let is_file = std::fs::metadata(&canonical)
                            .ok()
                            .map(|meta| meta.is_file())
                            .unwrap_or(false);
                        if is_file {
                            request.file = Some(rel);
                        } else {
                            let mut pattern = rel;
                            if !pattern.ends_with('/') {
                                pattern.push('/');
                            }
                            request.file_pattern = Some(pattern);
                        }
                        request.path = None;
                    }
                }
            }
        }
        return;
    }

    let normalized = raw_path.trim_start_matches("./");
    if normalized == "." || normalized.is_empty() {
        request.path = None;
        return;
    }
    let candidate = session_root.join(normalized);
    let meta = std::fs::metadata(&candidate).ok();
    let is_file = meta.as_ref().map(|m| m.is_file()).unwrap_or(false);
    let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
    if is_file {
        request.file = Some(normalized.to_string());
    } else {
        let mut pattern = normalized.to_string();
        if is_dir && !pattern.ends_with('/') {
            pattern.push('/');
        }
        request.file_pattern = Some(pattern);
    }
    request.path = None;
}
