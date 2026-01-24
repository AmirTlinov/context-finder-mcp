use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    canonicalize_root, canonicalize_root_path, collect_relative_hints, env_root_override,
    hint_score_for_root, rel_path_string, resolve_root_from_absolute_hints, trimmed_non_empty,
};

use super::super::ContextFinderService;

pub(in crate::tools::dispatch) fn workspace_roots_preview(roots: &[PathBuf]) -> String {
    let preview = roots
        .iter()
        .take(3)
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if roots.len() > 3 {
        if preview.is_empty() {
            "...".to_string()
        } else {
            format!("{preview}, ...")
        }
    } else {
        preview
    }
}

fn root_outside_workspace_error(context: &str, root_display: &str, roots: &[PathBuf]) -> String {
    let roots_preview = workspace_roots_preview(roots);
    format!(
        "{context}: resolved root '{root_display}' is outside MCP workspace roots [{roots_preview}]."
    )
}

fn select_workspace_root_by_hints(roots: &[PathBuf], hints: &[String]) -> Option<PathBuf> {
    let mut best_score = 0usize;
    let mut best: Option<PathBuf> = None;
    let mut tied = false;

    for root in roots {
        let score = hint_score_for_root(root, hints);
        if score == 0 {
            continue;
        }
        if score > best_score {
            best_score = score;
            best = Some(root.clone());
            tied = false;
        } else if score == best_score {
            tied = true;
        }
    }

    if best_score == 0 || tied {
        None
    } else {
        best
    }
}

impl ContextFinderService {
    pub(in crate::tools::dispatch) async fn resolve_root(
        &self,
        raw_path: Option<&str>,
    ) -> Result<(PathBuf, String), String> {
        self.resolve_root_with_hints(raw_path, &[]).await
    }

    pub(in crate::tools::dispatch) async fn resolve_root_with_hints(
        &self,
        raw_path: Option<&str>,
        hints: &[String],
    ) -> Result<(PathBuf, String), String> {
        let (root, root_display) = self.resolve_root_impl_with_hints(raw_path, hints).await?;
        self.touch_daemon_best_effort(&root);
        Ok((root, root_display))
    }

    pub(in crate::tools::dispatch) async fn resolve_root_no_daemon_touch(
        &self,
        raw_path: Option<&str>,
    ) -> Result<(PathBuf, String), String> {
        self.resolve_root_with_hints_no_daemon_touch(raw_path, &[])
            .await
    }

    pub(in crate::tools::dispatch) async fn resolve_root_with_hints_no_daemon_touch(
        &self,
        raw_path: Option<&str>,
        hints: &[String],
    ) -> Result<(PathBuf, String), String> {
        self.resolve_root_impl_with_hints(raw_path, hints).await
    }

    async fn resolve_root_impl_with_hints(
        &self,
        raw_path: Option<&str>,
        hints: &[String],
    ) -> Result<(PathBuf, String), String> {
        if trimmed_non_empty(raw_path).is_none() {
            if let Some(message) = self.session.lock().await.root_mismatch_error() {
                return Err(message.to_string());
            }
        }

        if let Some(raw) = trimmed_non_empty(raw_path) {
            // `path` is frequently passed as a relative file hint (e.g. `README.md`), which should
            // be resolved against the *session root* (or workspace root), not the server process
            // cwd. Otherwise, a shared daemon/in-process server can appear "randomly" pointed at
            // the wrong repo when the process cwd differs from the caller's working directory.
            let raw = raw.trim();
            let raw_path = Path::new(raw);
            let (session_root, mcp_workspace_roots) = {
                let session = self.session.lock().await;
                (session.clone_root(), session.mcp_workspace_roots().to_vec())
            };

            // Sticky-root safety: if a session root is already established, treat absolute `path`
            // values as file/dir *hints within the current project*, not as an implicit project
            // switch. This prevents accidental cross-project jumps when clients report absolute
            // file paths (editors) or when a caller passes `/etc/...` by mistake.
            if !self.allow_cwd_root_fallback && raw_path.is_absolute() {
                if let Some((root, root_display)) = session_root.as_ref() {
                    let session_root_allowed_by_workspace = mcp_workspace_roots.is_empty()
                        || mcp_workspace_roots
                            .iter()
                            .any(|candidate| root.starts_with(candidate));
                    if session_root_allowed_by_workspace {
                        let canonical = PathBuf::from(raw)
                            .canonicalize()
                            .map_err(|err| format!("Invalid path: {err}"))?;
                        if !canonical.starts_with(root) {
                            return Err(
                            "Invalid path: absolute `path` is outside the current project; call root_set to switch projects."
                                .to_string(),
                        );
                        }

                        let focus_file = std::fs::metadata(&canonical)
                            .ok()
                            .filter(|meta| meta.is_file())
                            .and_then(|_| canonical.strip_prefix(root).ok())
                            .and_then(rel_path_string);

                        let mut session = self.session.lock().await;
                        if self.allow_cwd_root_fallback || session.initialized() {
                            session.set_root(root.clone(), root_display.clone(), focus_file);
                        }
                        return Ok((root.clone(), root_display.clone()));
                    }
                }
            }

            let mut candidates: Vec<PathBuf> = Vec::new();
            if raw_path.is_absolute() {
                candidates.push(PathBuf::from(raw));
            } else if let Some((root, _)) = session_root.as_ref() {
                candidates.push(root.join(raw));
            } else if mcp_workspace_roots.len() == 1 {
                candidates.push(mcp_workspace_roots[0].join(raw));
            } else if mcp_workspace_roots.len() > 1 {
                // Multi-root workspace: try to disambiguate relative `path` against declared
                // workspace roots (safe) instead of immediately failing.
                let raw_norm = raw.replace('\\', "/");
                let raw_hint = vec![raw_norm.clone()];
                if let Some(workspace_root) =
                    select_workspace_root_by_hints(&mcp_workspace_roots, &raw_hint)
                {
                    candidates.push(workspace_root.join(raw_norm));
                } else {
                    return Err(
                        "Invalid path: relative `path` is ambiguous in a multi-root workspace; pass an absolute path or call root_set."
                            .to_string(),
                    );
                }
            } else if self.allow_cwd_root_fallback {
                // In-process server mode only: fall back to process cwd for relative paths.
                // Shared daemon mode must not guess across projects.
                candidates.push(PathBuf::from(raw));
            } else {
                return Err(
                    "Invalid path: relative `path` is ambiguous without a session/workspace root; pass an absolute path or initialize MCP roots."
                        .to_string(),
                );
            }

            let mut last_err: Option<String> = None;
            for candidate in candidates {
                match canonicalize_root_path(&candidate) {
                    Ok(root) => {
                        let root_display = root.to_string_lossy().to_string();

                        // Agent-native UX: callers often pass a "current file" path as `path`.
                        // Preserve the relative file hint (when possible) so `read_pack
                        // intent=memory` can surface the current working file without requiring
                        // extra parameters.
                        let mut focus_file: Option<String> = None;
                        if let Ok(canonical) = candidate.canonicalize() {
                            if let Ok(meta) = std::fs::metadata(&canonical) {
                                if meta.is_file() {
                                    if let Ok(rel) = canonical.strip_prefix(&root) {
                                        focus_file = rel_path_string(rel);
                                    }
                                }
                            }
                        }

                        let mut session = self.session.lock().await;
                        if !session.root_allowed_by_workspace(&root) {
                            last_err = Some(root_outside_workspace_error(
                                "Invalid path",
                                &root_display,
                                session.mcp_workspace_roots(),
                            ));
                            continue;
                        }
                        if self.allow_cwd_root_fallback || session.initialized() {
                            session.set_root(root.clone(), root_display.clone(), focus_file);
                        }
                        return Ok((root, root_display));
                    }

                    Err(err) => {
                        last_err = Some(format!("Invalid path: {err}"));
                    }
                }
            }

            return Err(last_err.unwrap_or_else(|| "Invalid path".to_string()));
        }

        // Sticky root: once a per-connection session root is established, do not implicitly
        // switch projects based on file/dir hints. This prevents accidental cross-project
        // contamination (e.g., absolute file paths like `/etc/passwd` or a different repo).
        {
            let session = self.session.lock().await;
            if let Some((root, root_display)) = session.clone_root() {
                if self.allow_cwd_root_fallback || session.initialized() {
                    if !session.root_allowed_by_workspace(&root) {
                        if let Some(message) = session.root_mismatch_error() {
                            return Err(message.to_string());
                        }
                        return Err(
                            "Missing project root: session root is outside MCP workspace roots; call `root_set` (recommended) or pass `path`."
                                .to_string(),
                        );
                    }
                    return Ok((root, root_display));
                }
            }
        }

        if let Some(root) = resolve_root_from_absolute_hints(hints) {
            let root_display = root.to_string_lossy().to_string();
            let mut session = self.session.lock().await;
            if !session.root_allowed_by_workspace(&root) {
                return Err(root_outside_workspace_error(
                    "Invalid path hint",
                    &root_display,
                    session.mcp_workspace_roots(),
                ));
            }
            if self.allow_cwd_root_fallback || session.initialized() {
                session.set_root(root.clone(), root_display.clone(), None);
            }
            return Ok((root, root_display));
        }

        let relative_hints = collect_relative_hints(hints);
        if !relative_hints.is_empty() {
            // Multi-root UX: when the client reports multiple MCP workspace roots, we normally
            // refuse to guess. However, many tool calls include file/dir hints (e.g. `file` in
            // read_pack). If those hints clearly match a single workspace root, we can
            // disambiguate safely and avoid forcing every caller to pass `path`.
            let workspace_roots = { self.session.lock().await.mcp_workspace_roots().to_vec() };
            if workspace_roots.len() > 1 {
                if let Some(root) =
                    select_workspace_root_by_hints(&workspace_roots, &relative_hints)
                {
                    let root_display = root.to_string_lossy().to_string();
                    let focus_file = relative_hints.iter().find_map(|hint| {
                        let candidate = root.join(hint);
                        std::fs::metadata(&candidate)
                            .ok()
                            .filter(|meta| meta.is_file())
                            .and_then(|_| rel_path_string(Path::new(hint)))
                    });
                    let mut session = self.session.lock().await;
                    if self.allow_cwd_root_fallback || session.initialized() {
                        session.set_root(root.clone(), root_display.clone(), focus_file);
                    }
                    return Ok((root, root_display));
                }
            }

            if self.allow_cwd_root_fallback {
                if let Some((root, root_display)) =
                    self.resolve_root_from_relative_hints(&relative_hints).await
                {
                    return Ok((root, root_display));
                }
            }
        }

        // Race guard: MCP roots are populated asynchronously after initialize. Some clients send
        // the first tool call immediately after initialize, before `roots/list` completes.
        //
        // In shared daemon mode, failing fast can accidentally route the call using stale session
        // state (when a transport is reused), or force clients to redundantly pass `path` even when
        // they support roots. Prefer a small bounded wait to let `roots/list` establish the
        // per-connection session root.
        let roots_pending = { self.session.lock().await.roots_pending() };
        if roots_pending {
            let wait_ms = if self.allow_cwd_root_fallback {
                150
            } else {
                900
            };
            let notify = self.roots_notify.clone();
            let _ = tokio::time::timeout(Duration::from_millis(wait_ms), notify.notified()).await;
            if let Some((root, root_display)) = self.session.lock().await.clone_root() {
                return Ok((root, root_display));
            }
        }

        if let Some((var, value)) = env_root_override() {
            let root = canonicalize_root(&value)
                .map_err(|err| format!("Invalid path from {var}: {err}"))?;
            let root_display = root.to_string_lossy().to_string();
            let mut session = self.session.lock().await;
            if !session.root_allowed_by_workspace(&root) {
                let context = format!("Invalid path from {var}");
                return Err(root_outside_workspace_error(
                    &context,
                    &root_display,
                    session.mcp_workspace_roots(),
                ));
            }
            if self.allow_cwd_root_fallback || session.initialized() {
                session.set_root(root.clone(), root_display.clone(), None);
            }
            return Ok((root, root_display));
        }

        if !self.allow_cwd_root_fallback {
            if self.session.lock().await.mcp_roots_ambiguous() {
                return Err(
                    "Missing project root: multiple MCP workspace roots detected; call `root_set` (recommended) or pass `path` to disambiguate."
                        .to_string(),
                );
            }
            return Err(
                "Missing project root: call `root_set` (recommended), or pass `path`, or enable MCP roots, or set CONTEXT_ROOT/CONTEXT_PROJECT_ROOT."
                    .to_string(),
            );
        }

        let cwd = env::current_dir()
            .map_err(|err| format!("Failed to determine current directory: {err}"))?;
        let candidate = cwd;
        let root =
            canonicalize_root_path(&candidate).map_err(|err| format!("Invalid path: {err}"))?;
        let root_display = root.to_string_lossy().to_string();
        let mut session = self.session.lock().await;
        if !session.root_allowed_by_workspace(&root) {
            return Err(root_outside_workspace_error(
                "Missing project root: computed cwd root",
                &root_display,
                session.mcp_workspace_roots(),
            ) + " Call `root_set` or pass `path`.");
        }
        if self.allow_cwd_root_fallback || session.initialized() {
            session.set_root(root.clone(), root_display.clone(), None);
        }
        Ok((root, root_display))
    }

    async fn resolve_root_from_relative_hints(
        &self,
        hints: &[String],
    ) -> Option<(PathBuf, String)> {
        let session_root = self.session.lock().await.clone_root();
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some((root, _)) = session_root.as_ref() {
            roots.push(root.clone());
        }
        for root in self.state.recent_roots().await {
            if !roots.iter().any(|known| known == &root) {
                roots.push(root);
            }
        }
        if roots.is_empty() {
            return None;
        }

        let mut best_score = 0usize;
        let mut best_roots: Vec<PathBuf> = Vec::new();
        for root in &roots {
            let score = hint_score_for_root(root, hints);
            if score == 0 {
                continue;
            }
            if score > best_score {
                best_score = score;
                best_roots.clear();
            }
            if score == best_score {
                best_roots.push(root.clone());
            }
        }

        if best_score == 0 || best_roots.is_empty() {
            return None;
        }

        let chosen = if best_roots.len() == 1 {
            best_roots.remove(0)
        } else if let Some((root, _)) = session_root {
            if best_roots.iter().any(|candidate| candidate == &root) {
                root
            } else {
                return None;
            }
        } else {
            return None;
        };

        let root_display = chosen.to_string_lossy().to_string();
        let mut session = self.session.lock().await;
        if !session.root_allowed_by_workspace(&chosen) {
            return None;
        }
        session.set_root(chosen.clone(), root_display.clone(), None);
        Some((chosen, root_display))
    }
}
