use super::root::{canonicalize_root_path, root_path_from_mcp_uri, workspace_roots_preview};
use super::{root::RootUpdateSource, router, ContextFinderService, ServiceState};
use crate::tools::catalog;
use context_search::SearchProfile;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{tool_handler, ErrorData as McpError, ServerHandler};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

impl ContextFinderService {
    pub fn new() -> Self {
        Self::new_with_policy(true)
    }

    pub fn new_daemon() -> Self {
        // Shared daemon mode: never guess a root from the daemon process cwd. Require either:
        // - explicit `path` on a tool call, or
        // - MCP roots capability (via initialize -> roots/list), or
        // - an explicit env override (CONTEXT_ROOT/CONTEXT_PROJECT_ROOT).
        Self::new_with_policy(false)
    }

    fn new_with_policy(allow_cwd_root_fallback: bool) -> Self {
        Self {
            profile: load_profile_from_env(),
            tool_router: router::build_tool_router_with_param_hints(),
            state: Arc::new(ServiceState::new()),
            session: Arc::new(Mutex::new(super::root::SessionDefaults::default())),
            roots_notify: Arc::new(Notify::new()),
            allow_cwd_root_fallback,
        }
    }

    pub fn clone_for_connection(&self) -> Self {
        Self {
            profile: self.profile.clone(),
            tool_router: self.tool_router.clone(),
            state: self.state.clone(),
            session: Arc::new(Mutex::new(super::root::SessionDefaults::default())),
            roots_notify: Arc::new(Notify::new()),
            allow_cwd_root_fallback: self.allow_cwd_root_fallback,
        }
    }
}

fn load_profile_from_env() -> SearchProfile {
    let profile_name = std::env::var("CONTEXT_PROFILE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "quality".to_string());

    if let Some(profile) = SearchProfile::builtin(&profile_name) {
        return profile;
    }

    let candidate_path = PathBuf::from(&profile_name);
    if candidate_path.exists() {
        match SearchProfile::from_file(&profile_name, &candidate_path) {
            Ok(profile) => return profile,
            Err(err) => {
                log::warn!(
                    "Failed to load profile from {}: {err:#}; falling back to builtin 'quality'",
                    candidate_path.display()
                );
            }
        }
    } else {
        log::warn!("Unknown profile '{profile_name}', falling back to builtin 'quality'");
    }

    SearchProfile::builtin("quality").unwrap_or_else(SearchProfile::general)
}

#[tool_handler]
impl ServerHandler for ContextFinderService {
    #[allow(clippy::manual_async_fn)]
    fn initialize(
        &self,
        request: rmcp::model::InitializeRequestParam,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<
        Output = std::result::Result<rmcp::model::InitializeResult, McpError>,
    > + Send
           + '_ {
        async move {
            // Treat every initialize as a fresh logical MCP session. Some MCP clients reuse a
            // long-lived server process (and/or transport) across multiple sessions, possibly in
            // different working directories. Without a reset, the daemon can retain a previous
            // session root and accidentally serve tool calls against the wrong project.
            {
                let mut session = self.session.lock().await;
                session.reset_for_initialize(request.capabilities.roots.is_some());
            }

            // Codex MCP client may be strict about the protocolVersion it requested during
            // initialization. rmcp defaults can lag behind, even when the tool surface is compatible.
            //
            // Agent-native behavior: echo the client's requested protocolVersion in the initialize
            // result so the transport stays open.
            if context.peer.peer_info().is_none() {
                context.peer.set_peer_info(request.clone());
            }

            // Session root: prefer the client's declared workspace roots when available.
            //
            // Important: do NOT block the initialize handshake on roots/list. Some MCP clients
            // cannot serve server->client requests until after initialization completes, and
            // blocking here can cause startup timeouts ("context deadline exceeded").
            if request.capabilities.roots.is_some() {
                let peer = context.peer.clone();
                let session = self.session.clone();
                let roots_notify = self.roots_notify.clone();
                tokio::spawn(async move {
                    // Give the client a moment to process the initialize response first.
                    tokio::time::sleep(Duration::from_millis(25)).await;

                    let roots = tokio::time::timeout(Duration::from_millis(800), peer.list_roots())
                        .await
                        .ok()
                        .and_then(|r| r.ok());

                    let mut candidates: Vec<PathBuf> = Vec::new();
                    if let Some(roots) = roots.as_ref() {
                        for root in &roots.roots {
                            let Some(path) = root_path_from_mcp_uri(&root.uri) else {
                                continue;
                            };
                            match canonicalize_root_path(&path) {
                                Ok(root) => candidates.push(root),
                                Err(err) => {
                                    log::debug!("Ignoring invalid MCP root {path:?}: {err}");
                                }
                            }
                        }
                    }
                    candidates.sort();
                    candidates.dedup();

                    let mut session = session.lock().await;
                    session.set_mcp_workspace_roots(candidates.clone());

                    // Workspace roots are the authoritative boundary when the client declares
                    // roots support.
                    if let Some((existing_root, existing_display)) = session.clone_root() {
                        if !session.root_allowed_by_workspace(&existing_root) {
                            let roots_preview = workspace_roots_preview(&candidates);
                            session.set_root_mismatch_error(format!(
                                "Missing project root: session root '{existing_display}' is outside MCP workspace roots [{roots_preview}]. Call `root_set` (recommended) or pass an explicit `path` within the workspace, or restart the session."
                            ));
                        }
                    } else {
                        match candidates.len() {
                            1 => {
                                let root = candidates.remove(0);
                                let root_display = root.to_string_lossy().to_string();
                                session.set_root(
                                    root,
                                    root_display,
                                    None,
                                    RootUpdateSource::McpRoots,
                                    None,
                                    None,
                                );
                            }
                            n if n > 1 => {
                                // Fail-closed: do not guess a root when the workspace is multi-root.
                                // This prevents cross-project contamination in shared-backend mode.
                                session.set_mcp_roots_ambiguous(true);
                            }
                            _ => {}
                        }
                    }
                    session.set_roots_pending(false);
                    drop(session);
                    roots_notify.notify_waiters();
                });
            }

            let mut info = self.get_info();
            info.protocol_version = request.protocol_version;
            Ok(info)
        }
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(catalog::tool_instructions()),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            ..Default::default()
        }
    }
}
