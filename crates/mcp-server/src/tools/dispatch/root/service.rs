use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    canonicalize_root, canonicalize_root_path, collect_relative_hints, env_root_override,
    hint_score_for_root, rel_path_string, resolve_root_from_absolute_hints, trimmed_non_empty,
    RootUpdateSnapshot, RootUpdateSource,
};

use super::super::ContextFinderService;
use crate::tools::util::truncate_to_chars;

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

struct RootDiagnostics {
    session_root: Option<String>,
    last_root_set: Option<RootUpdateSnapshot>,
    last_root_update: Option<RootUpdateSnapshot>,
    cwd: Option<String>,
}

impl RootDiagnostics {
    async fn capture(service: &ContextFinderService) -> Self {
        let (session_root, last_root_set, last_root_update) = {
            let session = service.session.lock().await;
            (
                session.root_display(),
                session.last_root_set_snapshot(),
                session.last_root_update_snapshot(),
            )
        };
        let cwd = env::current_dir()
            .ok()
            .map(|path| path.to_string_lossy().to_string());
        Self {
            session_root,
            last_root_set,
            last_root_update,
            cwd,
        }
    }

    fn update_json(update: &RootUpdateSnapshot) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        out.insert("source".to_string(), serde_json::json!(update.source));
        out.insert("at_ms".to_string(), serde_json::json!(update.at_ms));
        if let Some(path) = update.requested_path.as_deref() {
            out.insert("requested_path".to_string(), serde_json::json!(path));
        }
        if let Some(tool) = update.source_tool.as_deref() {
            out.insert("source_tool".to_string(), serde_json::json!(tool));
        }
        serde_json::Value::Object(out)
    }

    fn to_json(&self) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        if let Some(root) = self.session_root.as_deref() {
            out.insert("session_root".to_string(), serde_json::json!(root));
        }
        if let Some(cwd) = self.cwd.as_deref() {
            out.insert("cwd".to_string(), serde_json::json!(cwd));
        }
        if let Some(update) = self.last_root_set.as_ref() {
            out.insert("last_root_set".to_string(), Self::update_json(update));
        }
        if let Some(update) = self.last_root_update.as_ref() {
            let should_emit = match self.last_root_set.as_ref() {
                Some(last) => last.at_ms != update.at_ms,
                None => true,
            };
            if should_emit {
                out.insert("last_root_update".to_string(), Self::update_json(update));
            }
        }
        if out.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::Object(out)
        }
    }

    fn render_update(update: &RootUpdateSnapshot) -> String {
        let mut out = format!("{}@{}", update.source, update.at_ms);
        if let Some(path) = update.requested_path.as_deref() {
            let trimmed = truncate_to_chars(path, 140);
            out.push_str(&format!(" path={trimmed}"));
        }
        if let Some(tool) = update.source_tool.as_deref() {
            out.push_str(&format!(" tool={tool}"));
        }
        out
    }

    fn decorate_invalid_path(&self, message: String) -> String {
        if !message.starts_with("Invalid path") {
            return message;
        }

        let mut notes = Vec::new();
        if let Some(root) = self.session_root.as_deref() {
            notes.push(format!("session_root={root}"));
        }
        if let Some(cwd) = self.cwd.as_deref() {
            notes.push(format!("cwd={cwd}"));
        }
        if let Some(update) = self.last_root_set.as_ref() {
            notes.push(format!("last_root_set={}", Self::render_update(update)));
        }
        if let Some(update) = self.last_root_update.as_ref() {
            let should_emit = match self.last_root_set.as_ref() {
                Some(last) => last.at_ms != update.at_ms,
                None => true,
            };
            if should_emit {
                notes.push(format!("last_root_update={}", Self::render_update(update)));
            }
        }
        if let Some(cwd) = self.cwd.as_deref() {
            if self.session_root.as_deref() != Some(cwd) {
                notes.push(format!("hint=root_set path={cwd}"));
            }
        }
        if notes.is_empty() {
            message
        } else {
            format!("{message} ({})", notes.join("; "))
        }
    }
}

async fn decorate_invalid_path_error(service: &ContextFinderService, message: String) -> String {
    let diagnostics = RootDiagnostics::capture(service).await;
    diagnostics.decorate_invalid_path(message)
}

pub(in crate::tools::dispatch) async fn root_context_details(
    service: &ContextFinderService,
) -> serde_json::Value {
    RootDiagnostics::capture(service).await.to_json()
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
    pub(in crate::tools::dispatch) async fn resolve_root_for_tool(
        &self,
        raw_path: Option<&str>,
        tool: &'static str,
    ) -> Result<(PathBuf, String), String> {
        self.resolve_root_with_hints_for_tool(raw_path, &[], tool)
            .await
    }

    pub(in crate::tools::dispatch) async fn resolve_root_with_hints_for_tool(
        &self,
        raw_path: Option<&str>,
        hints: &[String],
        tool: &'static str,
    ) -> Result<(PathBuf, String), String> {
        let (root, root_display) = self
            .resolve_root_impl_with_hints(raw_path, hints, Some(tool))
            .await?;
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

    pub(in crate::tools::dispatch) async fn resolve_root_no_daemon_touch_for_tool(
        &self,
        raw_path: Option<&str>,
        tool: &'static str,
    ) -> Result<(PathBuf, String), String> {
        self.resolve_root_with_hints_no_daemon_touch_for_tool(raw_path, &[], tool)
            .await
    }

    pub(in crate::tools::dispatch) async fn resolve_root_with_hints_no_daemon_touch(
        &self,
        raw_path: Option<&str>,
        hints: &[String],
    ) -> Result<(PathBuf, String), String> {
        self.resolve_root_impl_with_hints(raw_path, hints, None)
            .await
    }

    pub(in crate::tools::dispatch) async fn resolve_root_with_hints_no_daemon_touch_for_tool(
        &self,
        raw_path: Option<&str>,
        hints: &[String],
        tool: &'static str,
    ) -> Result<(PathBuf, String), String> {
        self.resolve_root_impl_with_hints(raw_path, hints, Some(tool))
            .await
    }

    async fn resolve_root_impl_with_hints(
        &self,
        raw_path: Option<&str>,
        hints: &[String],
        source_tool: Option<&'static str>,
    ) -> Result<(PathBuf, String), String> {
        if trimmed_non_empty(raw_path).is_none() {
            if let Some(message) = self.session.lock().await.root_mismatch_error() {
                return Err(message.to_string());
            }
        }

        let requested_path = trimmed_non_empty(raw_path).map(str::to_string);
        let source_tool = source_tool.map(str::to_string);

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
                        let canonical = match PathBuf::from(raw).canonicalize() {
                            Ok(value) => value,
                            Err(err) => {
                                return Err(decorate_invalid_path_error(
                                    self,
                                    format!("Invalid path: {err}"),
                                )
                                .await);
                            }
                        };
                        if !canonical.starts_with(root) {
                            return Err(
                                decorate_invalid_path_error(
                                    self,
                                    "Invalid path: absolute `path` is outside the current project; call root_set to switch projects."
                                        .to_string(),
                                )
                                .await,
                            );
                        }

                        let focus_file = std::fs::metadata(&canonical)
                            .ok()
                            .filter(|meta| meta.is_file())
                            .and_then(|_| canonical.strip_prefix(root).ok())
                            .and_then(rel_path_string);

                        let mut session = self.session.lock().await;
                        if self.allow_cwd_root_fallback || session.initialized() {
                            session.set_root(
                                root.clone(),
                                root_display.clone(),
                                focus_file,
                                RootUpdateSource::ResolvePath,
                                requested_path.clone(),
                                source_tool.clone(),
                            );
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
                        decorate_invalid_path_error(
                            self,
                            "Invalid path: relative `path` is ambiguous in a multi-root workspace; pass an absolute path or call root_set."
                                .to_string(),
                        )
                        .await,
                    );
                }
            } else if self.allow_cwd_root_fallback {
                // In-process server mode only: fall back to process cwd for relative paths.
                // Shared daemon mode must not guess across projects.
                candidates.push(PathBuf::from(raw));
            } else {
                return Err(
                    decorate_invalid_path_error(
                        self,
                        "Invalid path: relative `path` is ambiguous without a session/workspace root; pass an absolute path or initialize MCP roots."
                            .to_string(),
                    )
                    .await,
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
                            session.set_root(
                                root.clone(),
                                root_display.clone(),
                                focus_file,
                                RootUpdateSource::ResolvePath,
                                requested_path.clone(),
                                source_tool.clone(),
                            );
                        }
                        return Ok((root, root_display));
                    }

                    Err(err) => {
                        last_err = Some(format!("Invalid path: {err}"));
                    }
                }
            }

            return Err(decorate_invalid_path_error(
                self,
                last_err.unwrap_or_else(|| "Invalid path".to_string()),
            )
            .await);
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
                return Err(decorate_invalid_path_error(
                    self,
                    root_outside_workspace_error(
                        "Invalid path hint",
                        &root_display,
                        session.mcp_workspace_roots(),
                    ),
                )
                .await);
            }
            if self.allow_cwd_root_fallback || session.initialized() {
                session.set_root(
                    root.clone(),
                    root_display.clone(),
                    None,
                    RootUpdateSource::ResolvePath,
                    None,
                    source_tool.clone(),
                );
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
                        session.set_root(
                            root.clone(),
                            root_display.clone(),
                            focus_file,
                            RootUpdateSource::ResolvePath,
                            None,
                            source_tool.clone(),
                        );
                    }
                    return Ok((root, root_display));
                }
            }

            if self.allow_cwd_root_fallback {
                if let Some((root, root_display)) = self
                    .resolve_root_from_relative_hints(&relative_hints, source_tool.as_deref())
                    .await
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
            let root = match canonicalize_root(&value) {
                Ok(value) => value,
                Err(err) => {
                    return Err(decorate_invalid_path_error(
                        self,
                        format!("Invalid path from {var}: {err}"),
                    )
                    .await);
                }
            };
            let root_display = root.to_string_lossy().to_string();
            let mut session = self.session.lock().await;
            if !session.root_allowed_by_workspace(&root) {
                let context = format!("Invalid path from {var}");
                return Err(decorate_invalid_path_error(
                    self,
                    root_outside_workspace_error(
                        &context,
                        &root_display,
                        session.mcp_workspace_roots(),
                    ),
                )
                .await);
            }
            if self.allow_cwd_root_fallback || session.initialized() {
                session.set_root(
                    root.clone(),
                    root_display.clone(),
                    None,
                    RootUpdateSource::EnvOverride,
                    Some(value),
                    source_tool.clone(),
                );
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
        let root = match canonicalize_root_path(&candidate) {
            Ok(root) => root,
            Err(err) => {
                return Err(
                    decorate_invalid_path_error(self, format!("Invalid path: {err}")).await,
                );
            }
        };
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
            session.set_root(
                root.clone(),
                root_display.clone(),
                None,
                RootUpdateSource::CwdFallback,
                None,
                source_tool.clone(),
            );
        }
        Ok((root, root_display))
    }

    async fn resolve_root_from_relative_hints(
        &self,
        hints: &[String],
        source_tool: Option<&str>,
    ) -> Option<(PathBuf, String)> {
        let source_tool = source_tool.map(str::to_string);
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
        session.set_root(
            chosen.clone(),
            root_display.clone(),
            None,
            RootUpdateSource::ResolvePath,
            None,
            source_tool,
        );
        Some((chosen, root_display))
    }
}
