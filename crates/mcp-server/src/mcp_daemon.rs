use crate::stdio_hybrid::{
    stdio_hybrid_server, EofDrainTransport, HybridStdioTransport, InitializedCompatTransport,
};
use crate::tools::ContextFinderService;
use anyhow::{Context, Result};
use context_vector_store::{CONTEXT_DIR_NAME, LEGACY_CONTEXT_DIR_NAME};
use rmcp::service::TxJsonRpcMessage;
use rmcp::transport::Transport;
use rmcp::ServiceExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};

type DaemonTransport = HybridStdioTransport<
    rmcp::RoleClient,
    tokio::io::ReadHalf<UnixStream>,
    tokio::io::WriteHalf<UnixStream>,
>;

const DEFAULT_STARTUP_WAIT_MS: u64 = 50;
// Keep daemon startup bounded: MCP clients often use ~10s startup timeouts.
// We prefer a fast, predictable start over waiting long enough to trigger client timeouts.
const DEFAULT_STARTUP_RETRIES: usize = 60; // ~3s

// Single-instance robustness:
//
// Multi-agent sessions can start at the same time (race), and the daemon may take a moment to
// reach `listen()` (Tokio runtime init, filesystem probes, etc). A too-small window makes other
// starters treat the socket as stale, delete it, and accidentally start a second daemon.
//
// We bias toward "wait a bit longer" instead of spawning duplicates.
const SINGLE_INSTANCE_WAIT_MS: u64 = 25;
// Keep this short: bind-side stale socket recovery should be fast, and MCP test harnesses
// expect the daemon to become connectable quickly.
const SINGLE_INSTANCE_RETRIES: usize = 12; // ~300ms
const SYNTH_INIT_ID: &str = "__cf_synth_init__";

// Detect and recover from "connectable but unresponsive" daemons (e.g., stopped by SIGSTOP).
// Keep this well below typical MCP client startup timeouts (~10s).
const DAEMON_PROBE_TIMEOUT_MS: u64 = 900;
const DAEMON_PROBE_ID: &str = "__cf_daemon_probe__";
const DAEMON_RECENT_START_WINDOW_MS: u64 = 2_000;

fn daemon_lock_path(socket: &Path) -> PathBuf {
    socket.with_extension("lock")
}

struct DaemonSpawnLock(std::fs::File);

impl Drop for DaemonSpawnLock {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

async fn acquire_daemon_spawn_lock(socket: &Path) -> Result<DaemonSpawnLock> {
    let lock_path = daemon_lock_path(socket);
    tokio::task::spawn_blocking(move || -> Result<DaemonSpawnLock> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open daemon lock file at {}", lock_path.display()))?;
        let fd = file.as_raw_fd();
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("acquire daemon lock at {}", lock_path.display()));
        }
        write_spawn_lock_metadata(&mut file);
        Ok(DaemonSpawnLock(file))
    })
    .await
    .context("join daemon lock task")?
}

async fn try_acquire_daemon_spawn_lock(socket: &Path) -> Result<Option<DaemonSpawnLock>> {
    let lock_path = daemon_lock_path(socket);
    tokio::task::spawn_blocking(move || -> Result<Option<DaemonSpawnLock>> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open daemon lock file at {}", lock_path.display()))?;
        let fd = file.as_raw_fd();
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            write_spawn_lock_metadata(&mut file);
            return Ok(Some(DaemonSpawnLock(file)));
        }

        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        Err(err).with_context(|| format!("try-acquire daemon lock at {}", lock_path.display()))
    })
    .await
    .context("join daemon try-lock task")?
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SpawnLockMetadata {
    pid: u32,
    exe: Option<String>,
    version: Option<String>,
    acquired_at_ms: Option<u64>,
}

fn write_spawn_lock_metadata(file: &mut std::fs::File) {
    // Best-effort: the lock itself is the primary mechanism; metadata is for recovery + debugging.
    let payload = SpawnLockMetadata {
        pid: std::process::id(),
        exe: self_exe_on_disk_path().map(|p| p.to_string_lossy().to_string()),
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        acquired_at_ms: system_time_to_unix_ms(std::time::SystemTime::now()),
    };

    if file.set_len(0).is_err() {
        return;
    }
    if file.seek(SeekFrom::Start(0)).is_err() {
        return;
    }
    let Ok(buf) = serde_json::to_vec(&payload) else {
        return;
    };
    if file.write_all(&buf).is_err() {
        return;
    }
    let _ = file.flush();
}

async fn read_spawn_lock_metadata(socket: &Path) -> Option<SpawnLockMetadata> {
    let lock_path = daemon_lock_path(socket);
    let bytes = tokio::fs::read(&lock_path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn pid_looks_like_context_mcp_process(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(cmdline) = tokio::fs::read(format!("/proc/{pid}/cmdline")).await else {
            return false;
        };
        let parts: Vec<&[u8]> = cmdline
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .collect();
        let joined = parts
            .iter()
            .map(|p| String::from_utf8_lossy(p).to_string())
            .collect::<Vec<_>>()
            .join(" ");
        (joined.contains("context-mcp") || joined.contains("context-finder-mcp"))
            && !joined.contains("daemon")
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

async fn recover_stuck_spawn_lock(socket: &Path) -> Option<()> {
    // Recovery policy:
    // - Only act when the spawn lock is busy *and* no daemon is connectable (caller already proved
    //   that), so we don't interfere with healthy backends.
    // - Only target a lock holder that is clearly our own proxy process *and* is in a stopped
    //   state (SIGSTOP / job control). This is a known failure mode: a stopped process can hold
    //   the flock forever, wedging daemon startup for every other session.
    // - Prefer SIGCONT first (non-destructive). If still stopped after a short grace period,
    //   terminate to unblock startup.
    let info = read_spawn_lock_metadata(socket).await?;
    let pid = info.pid as i32;
    if pid <= 0 {
        return None;
    }
    if !pid_looks_like_context_mcp_process(pid).await {
        return None;
    }
    if process_is_stopped(pid).await != Some(true) {
        return None;
    }

    if logging_enabled() {
        log::warn!(
            "MCP daemon spawn lock appears wedged by a stopped proxy (pid={pid}); attempting recovery"
        );
    }

    unsafe {
        let _ = libc::kill(pid, libc::SIGCONT);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    if process_is_stopped(pid).await == Some(true) {
        unsafe {
            let _ = libc::kill(pid, libc::SIGTERM);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        if process_alive(pid) {
            unsafe {
                let _ = libc::kill(pid, libc::SIGKILL);
            }
        }
    }

    Some(())
}

fn logging_enabled() -> bool {
    std::env::var("CONTEXT_MCP_LOG")
        .or_else(|_| std::env::var("CONTEXT_FINDER_MCP_LOG"))
        .ok()
        .map(|v| {
            let v = v.trim();
            !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
        })
        .unwrap_or(false)
}

fn default_socket_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let preferred = home.join(CONTEXT_DIR_NAME);
    let base = if preferred.exists() {
        preferred
    } else {
        let legacy = home.join(LEGACY_CONTEXT_DIR_NAME);
        if legacy.exists() {
            legacy
        } else {
            preferred
        }
    };
    base.join("mcp.sock")
}

pub fn socket_path_from_env() -> PathBuf {
    std::env::var("CONTEXT_MCP_SOCKET")
        .or_else(|_| std::env::var("CONTEXT_FINDER_MCP_SOCKET"))
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_socket_path)
}

pub async fn run_daemon(socket: &Path) -> Result<()> {
    // Startup is latency-critical: MCP clients often enforce strict server start timeouts.
    //
    // Keep the daemon "connectable + responsive" first. Environment/bootstrap work (model dir,
    // GPU libs) is handled lazily when semantic code paths are exercised.

    let listener = match bind_single_instance(socket).await? {
        Some(listener) => listener,
        None => return Ok(()), // already running
    };

    // Best-effort: persist PID for health checks / recovery.
    let _ = write_daemon_pid_file(socket).await;

    // Agent-native behavior: treat the shared daemon as "background infra".
    //
    // This makes it far less likely for background indexing or model warmups to steal CPU from
    // interactive work (editor, builds, tests) while keeping correctness and eventual freshness.
    best_effort_lower_daemon_priority();

    let service = ContextFinderService::new_daemon();
    // If the socket file gets unlinked/replaced (e.g., stale-socket recovery or manual cleanup),
    // the bound listener can stay alive but become unreachable by path. That can lead to
    // duplicate long-lived daemons.
    //
    // Agent-native behavior: detect "orphaned" daemons and exit cleanly so the newest daemon
    // becomes the only live backend.
    let bound_inode = socket_inode(socket);
    let mut watch = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            res = listener.accept() => {
                let (stream, _) = res?;
                let svc = service.clone_for_connection();
                tokio::spawn(async move {
                    if let Err(err) = serve_one_connection(svc, stream).await {
                        log::debug!("mcp daemon connection ended with error: {err:#}");
                    }
                });
            }
            _ = watch.tick() => {
                if let Some(bound_inode) = bound_inode {
                    match socket_inode(socket) {
                        Some(inode) if inode == bound_inode => {}
                        // Socket path was replaced -> we're orphaned.
                        Some(_) | None => break,
                    }
                } else if socket_inode(socket).is_none() {
                    // Socket path is gone -> we're orphaned.
                    // Socket path is gone -> we're orphaned.
                    break;
                }
            }
        }
    }
    Ok(())
}

fn best_effort_lower_daemon_priority() {
    // Lowering priority is best-effort and intentionally silent: failure to nice should not
    // prevent the daemon from starting.
    #[cfg(unix)]
    unsafe {
        // Positive nice values reduce CPU scheduling priority. Non-root users are allowed to
        // increase niceness (i.e., lower priority), which is exactly what we want here.
        let _ = libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
}

async fn serve_one_connection(service: ContextFinderService, stream: UnixStream) -> Result<()> {
    let (read, write) = tokio::io::split(stream);
    let transport =
        EofDrainTransport::new(InitializedCompatTransport::new(HybridStdioTransport::<
            rmcp::RoleServer,
            _,
            _,
        >::new(read, write)));
    let server = service.serve(transport).await?;
    server.waiting().await?;
    Ok(())
}

fn env_root_override() -> Option<PathBuf> {
    for key in [
        "CONTEXT_ROOT",
        "CONTEXT_PROJECT_ROOT",
        "CONTEXT_FINDER_ROOT",
        "CONTEXT_FINDER_PROJECT_ROOT",
    ] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }
    None
}

fn compute_proxy_default_root() -> Option<String> {
    let root = env_root_override().or_else(|| {
        let cwd = std::env::current_dir().ok()?;
        find_project_root(&cwd).or(Some(cwd))
    })?;
    let canonical = root.canonicalize().unwrap_or(root);
    Some(canonical.to_string_lossy().to_string())
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(PathBuf::from)
}

fn find_project_root(start: &Path) -> Option<PathBuf> {
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

fn ensure_tool_call_has_path(value: &mut Value, root: &str) -> bool {
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return false;
    };
    if method != "tools/call" {
        return false;
    }

    let Some(params) = value.get_mut("params").and_then(Value::as_object_mut) else {
        return false;
    };

    let args_value = params
        .entry("arguments")
        .or_insert_with(|| Value::Object(Map::new()));
    if !args_value.is_object() {
        *args_value = Value::Object(Map::new());
    }
    let Some(args) = args_value.as_object_mut() else {
        return false;
    };

    // Cursor continuation: many tools use cursor-only requests (no `path`) to resume pagination.
    // Injecting a default root would shadow cursor decoding and break pagination semantics.
    let has_cursor = args
        .get("cursor")
        .and_then(Value::as_str)
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    if has_cursor {
        return true;
    }

    let has_path = args
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .is_some_and(|v| !v.is_empty());
    if has_path {
        return true;
    }

    let trimmed_root = root.trim();
    if trimmed_root.is_empty() {
        return false;
    }

    args.insert("path".to_string(), Value::String(trimmed_root.to_string()));
    true
}

async fn connect_daemon_transport(socket: &Path) -> Result<DaemonTransport> {
    ensure_daemon(socket).await?;

    // Agent-native policy: treat probe timeouts as "busy" when the daemon process is alive.
    let probe = tokio::time::timeout(
        Duration::from_millis(DAEMON_PROBE_TIMEOUT_MS),
        probe_daemon_socket(socket),
    )
    .await;

    match probe {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            if logging_enabled() {
                log::warn!("MCP daemon probe failed; restarting ({err:#})");
            }
            force_restart_daemon(socket).await.ok();
        }
        Err(_) => {
            // If the daemon was *just* spawned, a probe timeout can simply mean "still warming up".
            if daemon_started_recently(socket, DAEMON_RECENT_START_WINDOW_MS).await {
                tokio::time::sleep(Duration::from_millis(50)).await;
            } else if should_restart_on_probe_timeout(socket).await {
                if logging_enabled() {
                    log::warn!("MCP daemon probe timed out; restarting");
                }
                force_restart_daemon(socket).await.ok();
            } else if logging_enabled() {
                log::warn!("MCP daemon probe timed out; continuing (daemon appears alive)");
            }
        }
    }

    // Probe succeeded; open a fresh connection for the actual MCP session so we don't poison the
    // handshake state of the session transport (probe uses handshake-less tools/list).
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to MCP daemon at {}", socket.display()))?;
    let (read, write) = tokio::io::split(stream);
    Ok(HybridStdioTransport::<rmcp::RoleClient, _, _>::new(
        read, write,
    ))
}

pub async fn proxy_stdio_to_daemon(socket: &Path) -> Result<()> {
    // Agent-native UX: allow `path` to be omitted even on the first tool call in shared mode.
    //
    // The shared daemon has its own working directory, so "missing path" would otherwise fall back
    // to an unrelated root (often the daemon's launch cwd). The per-session proxy runs inside the
    // agent session, so it can inject a correct default root for the first tool call that omits
    // `path`, establishing a per-connection session root inside the daemon.
    //
    // Important: cursor-only pagination calls must be left untouched, because tool routers decode
    // the root from the cursor when `path` is missing.
    let env_root = env_root_override().map(|root| {
        let canonical = root.canonicalize().unwrap_or(root);
        canonical.to_string_lossy().to_string()
    });
    let default_root = compute_proxy_default_root();

    // Hot-reload ergonomics:
    //
    // Agents often rebuild the `context-mcp` binary in-place (same path) without restarting the MCP
    // session. The long-lived shared daemon keeps running the old code, and the change looks like
    // "it didn't update" until someone manually restarts processes.
    //
    // Agent-native behavior: detect an in-place binary update (mtime change) and proactively reset
    // the daemon transport so the next tool call re-validates and (if needed) restarts the daemon.
    // We only do this when there are no in-flight requests to avoid breaking active tool calls.
    let mut last_exe_mtime_ms = current_exe_mtime_ms();
    let mut hot_restart_pending = false;
    let mut hot_restart_watch = tokio::time::interval(Duration::from_millis(500));
    hot_restart_watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut client_closed = false;

    #[derive(Default)]
    struct DaemonSessionState {
        root_established: bool,
        initialize_seen: bool,
        initialized_forwarded: bool,
        synthesized_initialize: bool,
        client_supports_roots: bool,
        client_protocol_version: Option<String>,
        initialize_request_id: Option<Value>,
        pending_request_ids: Vec<Value>,
        real_initialize_count: u64,
    }

    impl DaemonSessionState {
        fn reset_flags(&mut self) {
            self.root_established = false;
            self.initialize_seen = false;
            self.initialized_forwarded = false;
            self.synthesized_initialize = false;
            self.client_supports_roots = false;
            self.client_protocol_version = None;
            self.initialize_request_id = None;
        }
    }

    // Track in-flight request ids so we can fail fast if the backend drops mid-call.
    let mut session = DaemonSessionState::default();

    let mut client_transport = stdio_hybrid_server();
    let mut daemon_transport: Option<DaemonTransport> =
        Some(connect_daemon_transport(socket).await?);

    #[derive(Clone, Copy, Debug)]
    enum LoopStep {
        Continue,
        ResetDaemon,
        Shutdown,
    }

    async fn reply_backend_disconnected(
        client_transport: &mut HybridStdioTransport<
            rmcp::RoleServer,
            tokio::io::Stdin,
            tokio::io::Stdout,
        >,
        id: Value,
    ) {
        // Keep the payload tiny: this is a tight-loop UX path.
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32000,
                "message": "Backend daemon disconnected; retry the tool call."
            }
        });
        if let Ok(tx) = serde_json::from_value::<TxJsonRpcMessage<rmcp::RoleServer>>(value) {
            let _ = client_transport.send(tx).await;
        }
    }

    async fn fail_pending(
        client_transport: &mut HybridStdioTransport<
            rmcp::RoleServer,
            tokio::io::Stdin,
            tokio::io::Stdout,
        >,
        session: &mut DaemonSessionState,
    ) {
        while let Some(id) = session.pending_request_ids.pop() {
            reply_backend_disconnected(client_transport, id).await;
        }
    }

    async fn forward_client_value(
        daemon: &mut DaemonTransport,
        client_transport: &mut HybridStdioTransport<
            rmcp::RoleServer,
            tokio::io::Stdin,
            tokio::io::Stdout,
        >,
        default_root: Option<&str>,
        env_root: Option<&str>,
        value: &mut Value,
        session: &mut DaemonSessionState,
    ) -> Result<LoopStep> {
        let method = value.get("method").and_then(Value::as_str);
        let request_id = value.get("id").cloned().filter(|v| !v.is_null());

        // Compat: Some MCP clients skip or reorder `notifications/initialized`.
        // rmcp expects it right after `initialize`, so we synthesize it once.
        if session.synthesized_initialize && method == Some("initialize") {
            // Handshake-less clients may later send a real initialize; ignore to keep the daemon stable.
            return Ok(LoopStep::Continue);
        }

        if method == Some("initialize") {
            // Treat every initialize as a fresh logical MCP session (some clients reuse the same
            // server process across multiple sessions). Reset per-session defaults so the first
            // tool call can establish the correct root again.
            session.reset_flags();
            session.real_initialize_count = session.real_initialize_count.saturating_add(1);
            session.initialize_seen = true;
            session.initialize_request_id = request_id.clone();
            session.client_supports_roots = value
                .get("params")
                .and_then(Value::as_object)
                .and_then(|params| params.get("capabilities"))
                .and_then(Value::as_object)
                .and_then(|caps| caps.get("roots"))
                .is_some_and(|v| !v.is_null());
            if let Some(version) = value
                .get("params")
                .and_then(Value::as_object)
                .and_then(|params| params.get("protocolVersion"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
            {
                session.client_protocol_version = Some(version.to_string());
            }
        } else if method == Some("notifications/initialized") {
            if !session.initialize_seen {
                // Drop noise (initialized without initialize) to keep daemon session stable.
                return Ok(LoopStep::Continue);
            }
            if session.initialized_forwarded {
                return Ok(LoopStep::Continue);
            }
            session.initialized_forwarded = true;
        } else if session.initialize_seen && !session.initialized_forwarded {
            let init_not = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            });
            let tx_init: TxJsonRpcMessage<rmcp::RoleClient> = serde_json::from_value(init_not)
                .context("transcode synthesized notifications/initialized for daemon")?;
            if daemon.send(tx_init).await.is_err() {
                if let Some(id) = request_id {
                    reply_backend_disconnected(client_transport, id).await;
                }
                return Ok(LoopStep::ResetDaemon);
            }
            session.initialized_forwarded = true;
        }

        // Agent-native robustness: some tool runners call `tools/call` directly without handshake.
        if !session.initialize_seen && method != Some("notifications/initialized") {
            let init_req = serde_json::json!({
                "jsonrpc": "2.0",
                "id": SYNTH_INIT_ID,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "context-compat-proxy",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }
            });
            let tx_init: TxJsonRpcMessage<rmcp::RoleClient> = serde_json::from_value(init_req)
                .context("transcode synthesized initialize for daemon")?;
            if daemon.send(tx_init).await.is_err() {
                if let Some(id) = request_id {
                    reply_backend_disconnected(client_transport, id).await;
                }
                return Ok(LoopStep::ResetDaemon);
            }
            session.initialize_seen = true;
            session.synthesized_initialize = true;

            let init_not = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            });
            let tx_init: TxJsonRpcMessage<rmcp::RoleClient> = serde_json::from_value(init_not)
                .context("transcode synthesized notifications/initialized for daemon")?;
            if daemon.send(tx_init).await.is_err() {
                if let Some(id) = request_id {
                    reply_backend_disconnected(client_transport, id).await;
                }
                return Ok(LoopStep::ResetDaemon);
            }
            session.initialized_forwarded = true;
        }

        if !session.root_established {
            let mut established = false;
            if session.client_supports_roots {
                // When the client supports MCP roots, the daemon can establish the per-connection
                // root via `roots/list`. That is the most robust option for hosts that reuse a
                // single MCP server process across multiple sessions/projects.
                //
                // However, in the common CLI case (fresh proxy process; stable cwd) it's safer to
                // proactively pin the root on the first tool call: `roots/list` can be slow or
                // unavailable under load, and an unpinned session increases the chance of
                // accidental cross-project fallback behavior.
                if session.real_initialize_count <= 1 {
                    if let Some(root) = default_root {
                        established = ensure_tool_call_has_path(value, root);
                    }
                }
            } else {
                let root = if session.real_initialize_count > 1 {
                    env_root
                } else {
                    default_root
                };
                if let Some(root) = root {
                    established = ensure_tool_call_has_path(value, root);
                }
            }
            if established {
                session.root_established = true;
            }
        }

        let tx: TxJsonRpcMessage<rmcp::RoleClient> =
            serde_json::from_value(value.clone()).context("transcode client message for daemon")?;
        if daemon.send(tx).await.is_err() {
            if let Some(id) = request_id {
                reply_backend_disconnected(client_transport, id).await;
            }
            return Ok(LoopStep::ResetDaemon);
        }
        if let Some(id) = request_id {
            session.pending_request_ids.push(id);
        }

        Ok(LoopStep::Continue)
    }

    loop {
        // Agent-native robustness: some tool runners close stdin after sending a single request
        // (one-shot invocation) but still expect the response on stdout. Treat EOF on stdin as
        // "no more requests" rather than "disconnect", and keep the backend connection alive
        // until all pending responses are flushed.
        if client_closed && session.pending_request_ids.is_empty() {
            break;
        }

        let step = if let Some(daemon) = daemon_transport.as_mut() {
            tokio::select! {
                msg = client_transport.receive(), if !client_closed => {
                    match msg {
                        Some(msg) => {
                            let mut value =
                                serde_json::to_value(&msg).context("serialize client message")?;
                            forward_client_value(
                                daemon,
                                &mut client_transport,
                                default_root.as_deref(),
                                env_root.as_deref(),
                                &mut value,
                                &mut session,
                            )
                            .await?
                        }
                        None => {
                            client_closed = true;
                            LoopStep::Continue
                        }
                    }
                }
                msg = daemon.receive() => {
                    match msg {
                        Some(msg) => {
                            let mut value =
                                serde_json::to_value(&msg).context("serialize daemon message")?;
                            if session.synthesized_initialize
                                && value.get("id").and_then(Value::as_str) == Some(SYNTH_INIT_ID)
                            {
                                LoopStep::Continue
                            } else {
                                if let Some(id) = value.get("id") {
                                    if !id.is_null()
                                        && (value.get("result").is_some()
                                            || value.get("error").is_some())
                                    {
                                        session.pending_request_ids.retain(|pending| pending != id);
                                    }
                                }

                                // Codex MCP client is strict about the protocolVersion it requested
                                // during initialize. rmcp may respond with its own supported
                                // protocolVersion (often older), and some clients will close the
                                // transport even when the tool surface is compatible.
                                //
                                // Agent-native behavior: echo the client's requested
                                // `protocolVersion` in the initialize response payload.
                                if let (Some(init_id), Some(client_version)) = (
                                    session.initialize_request_id.as_ref(),
                                    session.client_protocol_version.as_ref(),
                                ) {
                                    if value.get("id") == Some(init_id)
                                        && value.get("result").is_some()
                                    {
                                        if let Some(result) =
                                            value.get_mut("result").and_then(Value::as_object_mut)
                                        {
                                            if result.contains_key("protocolVersion") {
                                                result.insert(
                                                    "protocolVersion".to_string(),
                                                    Value::String(client_version.clone()),
                                                );
                                            }
                                        }
                                    }
                                }
                                let tx: TxJsonRpcMessage<rmcp::RoleServer> =
                                    serde_json::from_value(value)
                                        .context("transcode daemon message for client")?;
                                match client_transport.send(tx).await {
                                    Ok(()) => LoopStep::Continue,
                                    Err(_) => LoopStep::Shutdown,
                                }
                            }
                        }
                        None => LoopStep::ResetDaemon,
                    }
                }
                _ = hot_restart_watch.tick() => {
                    let current = current_exe_mtime_ms();
                    if current > last_exe_mtime_ms {
                        last_exe_mtime_ms = current;
                        hot_restart_pending = true;
                    }

                    if hot_restart_pending && session.pending_request_ids.is_empty() {
                        hot_restart_pending = false;
                        LoopStep::ResetDaemon
                    } else {
                        LoopStep::Continue
                    }
                }
            }
        } else if client_closed {
            LoopStep::Shutdown
        } else {
            let msg = match client_transport.receive().await {
                Some(msg) => msg,
                None => return Ok(()),
            };

            match connect_daemon_transport(socket).await {
                Ok(mut new_daemon) => {
                    session.reset_flags();
                    let mut value =
                        serde_json::to_value(&msg).context("serialize client message")?;
                    let forward_step = forward_client_value(
                        &mut new_daemon,
                        &mut client_transport,
                        default_root.as_deref(),
                        env_root.as_deref(),
                        &mut value,
                        &mut session,
                    )
                    .await?;
                    daemon_transport = Some(new_daemon);
                    forward_step
                }
                Err(_) => {
                    let value = serde_json::to_value(&msg).context("serialize client message")?;
                    if let Some(id) = value.get("id").cloned().filter(|v| !v.is_null()) {
                        reply_backend_disconnected(&mut client_transport, id).await;
                    }
                    LoopStep::Continue
                }
            }
        };

        match step {
            LoopStep::Continue => {}
            LoopStep::ResetDaemon => {
                fail_pending(&mut client_transport, &mut session).await;
                daemon_transport = None;
                session.reset_flags();
                if client_closed {
                    break;
                }
            }
            LoopStep::Shutdown => break,
        }
    }

    let _ = client_transport.close().await;
    if let Some(mut daemon) = daemon_transport {
        let _ = daemon.close().await;
    }

    Ok(())
}

async fn daemon_started_recently(socket: &Path, window_ms: u64) -> bool {
    let pid_path = daemon_pid_path(socket);
    let Ok(bytes) = tokio::fs::read(&pid_path).await else {
        return false;
    };
    let Ok(info) = serde_json::from_slice::<DaemonPidFile>(&bytes) else {
        return false;
    };
    let Some(started_at_ms) = info.started_at_ms.filter(|ms| *ms > 0) else {
        return false;
    };

    let Some(now_ms) = system_time_to_unix_ms(std::time::SystemTime::now()) else {
        return false;
    };
    now_ms.saturating_sub(started_at_ms) <= window_ms
}

async fn should_restart_on_probe_timeout(socket: &Path) -> bool {
    let pid_path = daemon_pid_path(socket);
    let Ok(bytes) = tokio::fs::read(&pid_path).await else {
        // No daemon pid file -> cannot confirm it's a healthy CF daemon process.
        return true;
    };
    let Ok(info) = serde_json::from_slice::<DaemonPidFile>(&bytes) else {
        return true;
    };

    let pid = info.pid as i32;
    if pid <= 0 {
        return true;
    }
    if !process_alive(pid) {
        return true;
    }
    if pid_looks_like_daemon(pid).await.is_some_and(|ok| !ok) {
        return true;
    }
    if process_is_stopped(pid).await.is_some_and(|stopped| stopped) {
        return true;
    }
    false
}

async fn pid_looks_like_daemon(pid: i32) -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        let cmdline = tokio::fs::read(format!("/proc/{pid}/cmdline")).await.ok()?;
        let parts: Vec<&[u8]> = cmdline
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .collect();
        let joined = parts
            .iter()
            .map(|p| String::from_utf8_lossy(p).to_string())
            .collect::<Vec<_>>()
            .join(" ");
        Some(
            (joined.contains("context-mcp") || joined.contains("context-finder-mcp"))
                && joined.contains("daemon"),
        )
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::ensure_tool_call_has_path;
    use serde_json::json;

    #[test]
    fn does_not_touch_non_tool_calls() {
        let mut value = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        });
        assert!(!ensure_tool_call_has_path(&mut value, "/tmp/root"));
        assert!(value.get("params").and_then(|v| v.as_object()).is_some());
        assert!(value
            .get("params")
            .and_then(|v| v.get("arguments"))
            .is_none());
    }

    #[test]
    fn injects_path_when_missing() {
        let mut value = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "doctor",
                "arguments": {}
            }
        });
        assert!(ensure_tool_call_has_path(&mut value, "/tmp/root"));
        let args = value
            .get("params")
            .and_then(|v| v.get("arguments"))
            .and_then(|v| v.as_object())
            .expect("arguments object");
        assert_eq!(args.get("path").and_then(|v| v.as_str()), Some("/tmp/root"));
    }

    #[test]
    fn does_not_override_existing_path() {
        let mut value = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "doctor",
                "arguments": {
                    "path": "/tmp/explicit"
                }
            }
        });
        assert!(ensure_tool_call_has_path(&mut value, "/tmp/root"));
        let args = value
            .get("params")
            .and_then(|v| v.get("arguments"))
            .and_then(|v| v.as_object())
            .expect("arguments object");
        assert_eq!(
            args.get("path").and_then(|v| v.as_str()),
            Some("/tmp/explicit")
        );
    }

    #[test]
    fn does_not_inject_for_cursor_continuations() {
        let mut value = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "ls",
                "arguments": {
                    "cursor": "abcdef"
                }
            }
        });
        assert!(ensure_tool_call_has_path(&mut value, "/tmp/root"));
        let args = value
            .get("params")
            .and_then(|v| v.get("arguments"))
            .and_then(|v| v.as_object())
            .expect("arguments object");
        assert!(args.get("path").is_none());
    }

    #[test]
    fn injects_when_cursor_is_empty() {
        let mut value = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "ls",
                "arguments": {
                    "cursor": "   "
                }
            }
        });
        assert!(ensure_tool_call_has_path(&mut value, "/tmp/root"));
        let args = value
            .get("params")
            .and_then(|v| v.get("arguments"))
            .and_then(|v| v.as_object())
            .expect("arguments object");
        assert_eq!(args.get("path").and_then(|v| v.as_str()), Some("/tmp/root"));
    }

    #[test]
    fn normalizes_non_object_arguments() {
        let mut value = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "doctor",
                "arguments": "wat"
            }
        });
        assert!(ensure_tool_call_has_path(&mut value, "/tmp/root"));
        let args = value
            .get("params")
            .and_then(|v| v.get("arguments"))
            .and_then(|v| v.as_object())
            .expect("arguments object");
        assert_eq!(args.get("path").and_then(|v| v.as_str()), Some("/tmp/root"));
    }
}

async fn process_is_stopped(pid: i32) -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        // /proc/<pid>/stat: pid (comm) state ...
        let stat = tokio::fs::read_to_string(format!("/proc/{pid}/stat"))
            .await
            .ok()?;
        let end = stat.rfind(')')?;
        let after = stat.get(end + 1..)?.trim_start();
        let state = after.chars().next()?;
        Some(matches!(state, 'T' | 't'))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

pub async fn ensure_daemon(socket: &Path) -> Result<()> {
    // Fast path: when the socket is already connectable we should not block on the spawn lock.
    if UnixStream::connect(socket).await.is_ok() {
        if should_restart_running_daemon(socket).await {
            // Restart is a rare path; take the lock to avoid restart storms.
            let _spawn_lock = acquire_daemon_spawn_lock(socket).await?;
            ensure_daemon_locked(socket).await?;
        }
        return Ok(());
    }

    // Multi-session guard: only one process may decide a socket is stale and spawn the daemon.
    //
    // Principal-level UX constraint: this must never block long enough to trip MCP client startup
    // timeouts (often ~10s). Under high concurrency, a blocking lock would serialize startups and
    // push worst-case latency over the threshold.
    //
    // Agent-native approach:
    // - try-lock first; if another process is handling startup, just wait for the socket to become
    //   connectable (bounded), without contending on the lock.
    // - only take the blocking lock when we can actually act (spawn/recover stale socket).
    if let Some(_spawn_lock) = try_acquire_daemon_spawn_lock(socket).await? {
        return ensure_daemon_locked(socket).await;
    }

    // Another process is starting/recovering the daemon; wait briefly for it to become ready.
    for _ in 0..DEFAULT_STARTUP_RETRIES {
        if UnixStream::connect(socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(DEFAULT_STARTUP_WAIT_MS)).await;
    }

    // Still not connectable. Try one more time to become the "winner" who can recover stale state.
    if let Some(_spawn_lock) = try_acquire_daemon_spawn_lock(socket).await? {
        return ensure_daemon_locked(socket).await;
    }

    // Recovery: the spawn lock can be held forever if a proxy process is SIGSTOP'd mid-startup.
    // That wedges the shared backend and forces every other session into the expensive in-process
    // fallback. Attempt a best-effort recovery, then retry once.
    //
    // This is intentionally conservative: we only act on a *stopped* process that looks like our
    // own proxy binary, and only when no daemon is connectable.
    if recover_stuck_spawn_lock(socket).await.is_some() {
        for _ in 0..SINGLE_INSTANCE_RETRIES {
            if UnixStream::connect(socket).await.is_ok() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(SINGLE_INSTANCE_WAIT_MS)).await;
        }
        if let Some(_spawn_lock) = try_acquire_daemon_spawn_lock(socket).await? {
            return ensure_daemon_locked(socket).await;
        }
    }

    anyhow::bail!("MCP daemon startup in progress (lock busy)")
}

async fn ensure_daemon_locked(socket: &Path) -> Result<()> {
    if UnixStream::connect(socket).await.is_ok() {
        if should_restart_running_daemon(socket).await {
            if logging_enabled() {
                log::warn!("Restarting MCP daemon due to binary change");
            }
            restart_daemon_locked(socket).await?;
        }
        return Ok(());
    }

    // If the socket file already exists but connect fails, another process is likely starting
    // the daemon. Wait briefly before spawning to avoid start storms.
    if tokio::fs::metadata(socket).await.is_ok() {
        // Under load, daemon startup can take longer than the single-instance bind window.
        // Once we hold the spawn lock, it's safe to wait longer here without risking storms.
        for _ in 0..DEFAULT_STARTUP_RETRIES {
            if UnixStream::connect(socket).await.is_ok() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(DEFAULT_STARTUP_WAIT_MS)).await;
        }

        // Socket file exists but never became connectable -> treat as stale.
        // This can happen after unclean shutdowns or crashes. Removing it here prevents the
        // newly spawned daemon from wasting time waiting on an unconnectable path.
        let _ = tokio::fs::remove_file(socket).await;
    }

    spawn_daemon(socket).await
}

#[derive(Debug, Deserialize)]
struct DaemonPidFile {
    pid: u32,
    exe: Option<String>,
    version: Option<String>,
    started_at_ms: Option<u64>,
}

fn daemon_pid_path(socket: &Path) -> PathBuf {
    // Keep it colocated with the socket so overrides stay consistent.
    socket.with_extension("pid")
}

fn system_time_to_unix_ms(value: std::time::SystemTime) -> Option<u64> {
    value
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

fn current_exe_mtime_ms() -> u64 {
    // Prefer an on-disk path to the executable.
    //
    // Why: during `cargo build` / rebuilds, the running process keeps an open inode while the
    // target path is atomically replaced. `/proc/self/exe` then becomes `(... deleted)`, and
    // `current_exe()` tracks the old inode, so its mtime stops reflecting rebuilds. Watching
    // the on-disk path keeps the proxy/daemon hot-reload logic working and allows respawns.
    self_exe_on_disk_path()
        .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .and_then(system_time_to_unix_ms)
        .unwrap_or(0)
}

fn self_exe_on_disk_path() -> Option<PathBuf> {
    // Best-effort: we want a path that remains stable across atomic rebuilds (rename-over),
    // so we can both detect updates (mtime) and spawn the new binary after hot-restart.
    //
    // Order of attempts:
    // 1) `argv[0]` resolved to an existing file (often the build output path).
    // 2) `current_exe()` if it points to an existing file.
    // 3) `current_exe()` with the Linux ` (deleted)` suffix stripped (atomic replace case).

    if let Some(arg0) = std::env::args_os().next() {
        let p = PathBuf::from(arg0);
        if let Some(resolved) = resolve_exe_candidate(&p) {
            return Some(resolved);
        }
    }

    if let Ok(p) = std::env::current_exe() {
        if p.is_file() {
            return Some(p);
        }
        if let Some(stripped) = strip_deleted_suffix(&p) {
            if stripped.is_file() {
                return Some(stripped);
            }
        }
    }

    None
}

fn resolve_exe_candidate(candidate: &Path) -> Option<PathBuf> {
    if candidate.as_os_str().is_empty() {
        return None;
    }

    // If the candidate is already a path, resolve it against the current working directory.
    if candidate.is_absolute() || candidate.components().count() > 1 {
        let resolved = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            std::env::current_dir().ok()?.join(candidate)
        };
        if resolved.is_file() {
            return Some(resolved);
        }
        return None;
    }

    // Otherwise treat it as a program name and search PATH (like `which`).
    let name = candidate;
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }

    None
}

fn strip_deleted_suffix(path: &Path) -> Option<PathBuf> {
    // Linux-specific: `/proc/self/exe` may resolve to `... (deleted)` after an atomic replace.
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        const SUFFIX: &[u8] = b" (deleted)";
        let bytes = path.as_os_str().as_bytes();
        if bytes.ends_with(SUFFIX) {
            let trimmed = &bytes[..bytes.len().saturating_sub(SUFFIX.len())];
            return Some(PathBuf::from(std::ffi::OsString::from_vec(
                trimmed.to_vec(),
            )));
        }
    }

    let _ = path;
    None
}

async fn should_restart_running_daemon(socket: &Path) -> bool {
    let pid_path = daemon_pid_path(socket);
    let Ok(bytes) = tokio::fs::read(&pid_path).await else {
        return false;
    };
    let Ok(info) = serde_json::from_slice::<DaemonPidFile>(&bytes) else {
        return false;
    };

    // Agent-native robustness: avoid restart storms when multiple frontends run the same
    // tool version but launch from different binary paths (e.g. `cargo install` vs `target/release`).
    //
    // Restart only when we can confidently detect a version mismatch or a binary modification
    // after the daemon started.
    if let Some(version) = info
        .version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        if version != env!("CARGO_PKG_VERSION") {
            return true;
        }
    }

    let Some(started_at_ms) = info.started_at_ms.filter(|ms| *ms > 0) else {
        return false;
    };
    let current_exe_mtime_ms = current_exe_mtime_ms();

    let daemon_exe_mtime_ms = info
        .exe
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .and_then(system_time_to_unix_ms)
        .unwrap_or(0);

    daemon_exe_mtime_ms > started_at_ms || current_exe_mtime_ms > started_at_ms
}

async fn write_daemon_pid_file(socket: &Path) -> Result<()> {
    let pid_path = daemon_pid_path(socket);
    if let Some(parent) = pid_path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    let payload = serde_json::json!({
        "pid": std::process::id(),
        "exe": self_exe_on_disk_path().map(|p| p.to_string_lossy().to_string()),
        "version": env!("CARGO_PKG_VERSION"),
        "started_at_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    });

    tokio::fs::write(&pid_path, serde_json::to_vec(&payload)?)
        .await
        .context("write daemon pid file")?;
    Ok(())
}

async fn probe_daemon(daemon: &mut DaemonTransport) -> Result<()> {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": DAEMON_PROBE_ID,
        "method": "tools/list",
        "params": {}
    });
    let tx: TxJsonRpcMessage<rmcp::RoleClient> =
        serde_json::from_value(req).context("transcode daemon probe request")?;
    daemon.send(tx).await.context("send daemon probe request")?;

    loop {
        let Some(msg) = daemon.receive().await else {
            anyhow::bail!("daemon disconnected during probe");
        };
        let value = serde_json::to_value(&msg).context("serialize daemon probe response")?;
        let id = value.get("id").and_then(Value::as_str);
        if id == Some(DAEMON_PROBE_ID) {
            // Success: daemon responded to a basic MCP request.
            return Ok(());
        }
    }
}

async fn probe_daemon_socket(socket: &Path) -> Result<()> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to MCP daemon probe at {}", socket.display()))?;
    let (read, write) = tokio::io::split(stream);
    let mut transport = HybridStdioTransport::<rmcp::RoleClient, _, _>::new(read, write);
    probe_daemon(&mut transport).await?;
    let _ = transport.close().await;
    Ok(())
}

async fn force_restart_daemon(socket: &Path) -> Result<()> {
    let _spawn_lock = acquire_daemon_spawn_lock(socket).await?;

    force_restart_daemon_locked(socket).await
}

async fn force_restart_daemon_locked(socket: &Path) -> Result<()> {
    restart_daemon_locked(socket).await
}

async fn restart_daemon_locked(socket: &Path) -> Result<()> {
    // Best-effort: terminate the daemon if we can positively identify it. Otherwise, fall back to
    // unlinking the socket and starting a fresh daemon instance.
    try_kill_daemon_from_pid_file(socket).await;

    let _ = tokio::fs::remove_file(daemon_pid_path(socket)).await;
    let _ = tokio::fs::remove_file(socket).await;
    spawn_daemon(socket).await
}

async fn spawn_daemon(socket: &Path) -> Result<()> {
    if let Some(parent) = socket.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    let exe = self_exe_on_disk_path().context("resolve on-disk executable for daemon spawn")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null());

    if logging_enabled() {
        let log_path = socket.with_extension("log");
        if let Ok(file) = OpenOptions::new()
            .create(true)
            .append(true)
            .truncate(false)
            .open(&log_path)
        {
            let stdout = file
                .try_clone()
                .map(Stdio::from)
                .unwrap_or_else(|_| Stdio::null());
            cmd.stdout(stdout);
            cmd.stderr(Stdio::from(file));
        } else {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }

    // Detach the daemon process so tool-runner process-group cleanups do not kill the backend.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn().with_context(|| "failed to spawn MCP daemon")?;
    // Best-effort hygiene: if the daemon exits while the proxy stays alive, reap it to avoid
    // accumulating zombies in long-lived agent sessions.
    tokio::task::spawn_blocking(move || {
        let _ = child.wait();
    });

    let mut retries = 0;
    while retries < DEFAULT_STARTUP_RETRIES {
        if UnixStream::connect(socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(DEFAULT_STARTUP_WAIT_MS)).await;
        retries += 1;
    }
    anyhow::bail!("MCP daemon did not start in time")
}

async fn try_kill_daemon_from_pid_file(socket: &Path) {
    let pid_path = daemon_pid_path(socket);
    let Ok(bytes) = tokio::fs::read(&pid_path).await else {
        return;
    };
    let Ok(info) = serde_json::from_slice::<DaemonPidFile>(&bytes) else {
        return;
    };
    let pid = info.pid as i32;
    if pid <= 0 {
        return;
    }

    #[cfg(target_os = "linux")]
    {
        // Verify the PID is really our daemon process before sending signals.
        if let Ok(cmdline) = tokio::fs::read(format!("/proc/{pid}/cmdline")).await {
            let parts: Vec<&[u8]> = cmdline
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .collect();
            let joined = parts
                .iter()
                .map(|p| String::from_utf8_lossy(p).to_string())
                .collect::<Vec<_>>()
                .join(" ");
            let looks_like_daemon = (joined.contains("context-mcp")
                || joined.contains("context-finder-mcp"))
                && joined.contains("daemon");
            if !looks_like_daemon {
                return;
            }
        } else {
            return;
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        // On non-Linux platforms we don't have a reliable, dependency-free way to verify the PID.
        // Avoid killing an unrelated process; rely on socket unlink + respawn instead.
        return;
    }

    unsafe {
        let _ = libc::kill(pid, libc::SIGTERM);
    }
    for _ in 0..20 {
        if !process_alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    unsafe {
        let _ = libc::kill(pid, libc::SIGKILL);
    }
}

fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe {
        if libc::kill(pid, 0) == 0 {
            return true;
        }
        matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        )
    }
}

async fn bind_single_instance(socket_path: &Path) -> Result<Option<UnixListener>> {
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    match UnixListener::bind(socket_path) {
        Ok(listener) => return Ok(Some(listener)),
        Err(err) => {
            // If connect succeeds, another daemon is already listening.
            //
            // NOTE: `bind()` can create the socket file before the other daemon calls `listen()`.
            // A concurrent startup can therefore briefly see "bind failed" + "connect failed".
            // Treating that as stale and deleting the socket would kill the other daemon.
            //
            // Instead, wait for the socket to become connectable before assuming stale.
            for _ in 0..SINGLE_INSTANCE_RETRIES {
                if UnixStream::connect(socket_path).await.is_ok() {
                    return Ok(None);
                }
                tokio::time::sleep(Duration::from_millis(SINGLE_INSTANCE_WAIT_MS)).await;
            }
            log::debug!(
                "mcp daemon socket bind failed ({}), treating as stale and retrying: {err}",
                socket_path.display()
            );
        }
    }

    let _ = tokio::fs::remove_file(socket_path).await;
    Ok(Some(UnixListener::bind(socket_path).with_context(
        || format!("failed to bind {}", socket_path.display()),
    )?))
}

fn socket_inode(socket: &Path) -> Option<u64> {
    std::fs::metadata(socket).ok().map(|m| m.ino())
}
