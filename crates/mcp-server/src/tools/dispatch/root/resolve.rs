use std::env;
use std::path::{Path, PathBuf};

pub(in crate::tools::dispatch) fn resolve_root_from_absolute_hints(
    hints: &[String],
) -> Option<PathBuf> {
    for hint in hints {
        let trimmed = hint.trim();
        if trimmed.is_empty() {
            continue;
        }
        let path = Path::new(trimmed);
        if !path.is_absolute() {
            continue;
        }
        if let Ok(root) = canonicalize_root_path(path) {
            return Some(root);
        }
    }
    None
}

pub(in crate::tools::dispatch) fn collect_relative_hints(hints: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for hint in hints {
        let trimmed = hint.trim();
        if trimmed.is_empty() {
            continue;
        }
        let trimmed = trimmed.replace('\\', "/");
        if Path::new(&trimmed).is_absolute() {
            continue;
        }
        if is_glob_hint(&trimmed) {
            continue;
        }
        if !looks_like_path_hint(&trimmed) {
            continue;
        }
        out.push(trimmed);
        if out.len() >= 8 {
            break;
        }
    }
    out
}

pub(in crate::tools::dispatch) fn hint_score_for_root(root: &Path, hints: &[String]) -> usize {
    let mut score = 0usize;
    for hint in hints {
        if root.join(hint).exists() {
            score = score.saturating_add(1);
        }
    }
    score
}

#[derive(Debug, Clone)]
pub(in crate::tools::dispatch) struct ScopeHint {
    pub include_paths: Vec<String>,
    pub file_pattern: Option<String>,
}

pub(in crate::tools::dispatch) fn scope_hint_from_relative_path(
    session_root: &Path,
    raw_path: &str,
) -> Option<ScopeHint> {
    let normalized = raw_path.trim().replace('\\', "/");
    let normalized = normalized.trim_start_matches("./");
    let normalized = normalized.trim_end_matches('/');
    if normalized.is_empty() || normalized == "." {
        return None;
    }
    if Path::new(normalized).is_absolute() {
        return None;
    }

    if is_glob_hint(normalized) {
        return Some(ScopeHint {
            include_paths: Vec::new(),
            file_pattern: Some(normalized.to_string()),
        });
    }

    let candidate = session_root.join(normalized);
    let is_dir = std::fs::metadata(&candidate)
        .ok()
        .map(|meta| meta.is_dir())
        .unwrap_or(false);
    if is_dir {
        return Some(ScopeHint {
            include_paths: vec![normalized.to_string()],
            file_pattern: None,
        });
    }

    Some(ScopeHint {
        include_paths: Vec::new(),
        file_pattern: Some(normalized.to_string()),
    })
}

fn is_glob_hint(value: &str) -> bool {
    value.contains('*') || value.contains('?')
}

fn looks_like_path_hint(value: &str) -> bool {
    value.contains('/') || value.starts_with('.') || value.contains('.')
}

pub(in crate::tools::dispatch) fn env_root_override() -> Option<(String, String)> {
    for key in [
        "CONTEXT_ROOT",
        "CONTEXT_PROJECT_ROOT",
        "CONTEXT_FINDER_ROOT",
        "CONTEXT_FINDER_PROJECT_ROOT",
    ] {
        if let Ok(value) = env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some((key.to_string(), trimmed.to_string()));
            }
        }
    }
    None
}

pub(in crate::tools::dispatch) fn canonicalize_root(raw: &str) -> Result<PathBuf, String> {
    canonicalize_root_path(Path::new(raw))
}

pub(in crate::tools::dispatch) fn canonicalize_root_path(path: &Path) -> Result<PathBuf, String> {
    let canonical = path.canonicalize().map_err(|err| err.to_string())?;

    // Agent-native UX: callers often pass a "current file" path as `path`.
    // Treat that as a hint within the project and prefer the enclosing git root (when present),
    // otherwise fall back to the file's parent directory.
    let (base, is_file) = match std::fs::metadata(&canonical) {
        Ok(meta) if meta.is_file() => (
            canonical
                .parent()
                .map(PathBuf::from)
                .ok_or_else(|| "Invalid path: file has no parent directory".to_string())?,
            true,
        ),
        _ => (canonical, false),
    };

    if is_file {
        if let Some(project_root) = find_project_root(&base) {
            return Ok(project_root);
        }
    }

    Ok(base)
}

pub(in crate::tools::dispatch) fn rel_path_string(path: &Path) -> Option<String> {
    let raw = path.to_string_lossy().to_string();
    let normalized = raw.replace('\\', "/");
    let trimmed = normalized.trim().trim_start_matches("./");
    if trimmed.is_empty() || trimmed == "." {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(PathBuf::from)
}

pub(in crate::tools::dispatch) fn find_project_root(start: &Path) -> Option<PathBuf> {
    if let Some(root) = find_git_root(start) {
        return Some(root);
    }

    const MARKERS: &[&str] = &[
        "AGENTS.md",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "CMakeLists.txt",
        "Makefile",
    ];

    start
        .ancestors()
        .find(|candidate| MARKERS.iter().any(|marker| candidate.join(marker).exists()))
        .map(PathBuf::from)
}

pub(in crate::tools::dispatch) fn root_path_from_mcp_uri(uri: &str) -> Option<PathBuf> {
    let uri = uri.trim();
    if uri.is_empty() {
        return None;
    }

    // Only local file:// URIs are meaningful for a filesystem-indexing MCP server.
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_decode_utf8(rest)?;

    // file:///abs/path  -> "/abs/path"
    // file://localhost/abs/path -> "/abs/path"
    let decoded = decoded.strip_prefix("localhost").unwrap_or(&decoded);
    if !decoded.starts_with('/') {
        return None;
    }

    #[cfg(not(windows))]
    let path = decoded.to_string();

    // Windows file URIs are often "file:///C:/path" (leading slash before drive).
    #[cfg(windows)]
    let path = {
        let mut path = decoded.to_string();
        if path.len() >= 3
            && path.as_bytes()[0] == b'/'
            && path.as_bytes()[2] == b':'
            && path.as_bytes()[1].is_ascii_alphabetic()
        {
            path = path[1..].to_string();
        }
        path
    };

    Some(PathBuf::from(path))
}

fn percent_decode_utf8(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = *bytes.get(i + 1)?;
                let lo = *bytes.get(i + 2)?;
                let hi = (hi as char).to_digit(16)? as u8;
                let lo = (lo as char).to_digit(16)? as u8;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}
