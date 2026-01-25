use super::super::ProjectFactsResult;
use std::path::Path;

pub(super) fn recall_code_scope_candidates(root: &Path, facts: &ProjectFactsResult) -> Vec<String> {
    // A small, deterministic set of "likely code lives here" roots used as a second-pass scope
    // for precision grep (avoids README/docs-first matches when snippet_limit is tight).
    let mut out: Vec<String> = Vec::new();

    // Prefer project-specific knowledge when available (facts.key_dirs is already bounded).
    for dir in &facts.key_dirs {
        let dir = dir.trim().replace('\\', "/");
        if dir.is_empty() || dir.starts_with('.') {
            continue;
        }
        if matches!(
            dir.as_str(),
            "src"
                | "crates"
                | "packages"
                | "apps"
                | "services"
                | "lib"
                | "libs"
                | "backend"
                | "frontend"
                | "server"
                | "client"
        ) && root.join(&dir).is_dir()
        {
            out.push(dir);
        }
        if out.len() >= 6 {
            break;
        }
    }

    // Fallback: common container directories (covers thin wrappers where key_dirs is noisy).
    if out.is_empty() {
        for dir in [
            "src", "crates", "packages", "apps", "services", "lib", "libs",
        ] {
            if root.join(dir).is_dir() {
                out.push(dir.to_string());
            }
            if out.len() >= 6 {
                break;
            }
        }
    }

    out
}
