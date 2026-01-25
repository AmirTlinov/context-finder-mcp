use super::super::*;
use crate::test_support::ENV_MUTEX;
use crate::tools::dispatch::root::RootUpdateSource;
use tempfile::tempdir;
use tokio::time::Duration;

#[tokio::test]
async fn resolve_root_waits_for_initialize_roots_list() {
    let dir = tempdir().expect("temp dir");
    let canonical_root = dir.path().canonicalize().expect("canonical root");
    let canonical_root_clone = canonical_root.clone();
    let canonical_display = canonical_root.to_string_lossy().to_string();

    let service = ContextFinderService::new_daemon();
    {
        let mut session = service.session.lock().await;
        session.reset_for_initialize(true);
    }

    let session_arc = service.session.clone();
    let notify = service.roots_notify.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut session = session_arc.lock().await;
        session.set_root(
            canonical_root_clone,
            canonical_display,
            None,
            RootUpdateSource::RootSet,
            None,
            None,
        );
        session.set_roots_pending(false);
        drop(session);
        notify.notify_waiters();
    });

    let (root, _) = service
        .resolve_root_no_daemon_touch(None)
        .await
        .expect("root must resolve after roots/list");
    assert_eq!(root, canonical_root);
}

#[tokio::test]
async fn daemon_does_not_reuse_or_persist_root_without_initialize() {
    let dir = tempdir().expect("temp dir");
    let canonical_root = dir.path().canonicalize().expect("canonical root");
    let root_str = canonical_root.to_string_lossy().to_string();

    let service = ContextFinderService::new_daemon();

    let (resolved, _) = service
        .resolve_root_no_daemon_touch(Some(&root_str))
        .await
        .expect("root must resolve with explicit path");
    assert_eq!(resolved, canonical_root);

    let err = service
        .resolve_root_no_daemon_touch(None)
        .await
        .expect_err("expected missing root without initialize");
    assert!(
        err.contains("Missing project root"),
        "expected error to mention missing project root"
    );

    {
        let mut session = service.session.lock().await;
        session.reset_for_initialize(false);
    }

    let (resolved, _) = service
        .resolve_root_no_daemon_touch(Some(&root_str))
        .await
        .expect("root must resolve after initialize");
    assert_eq!(resolved, canonical_root);

    let (resolved, _) = service
        .resolve_root_no_daemon_touch(None)
        .await
        .expect("expected sticky root after initialize");
    assert_eq!(resolved, canonical_root);
}

#[tokio::test]
async fn daemon_refuses_cross_project_root_inference_from_relative_hints() {
    let root_a = tempdir().expect("temp root_a");
    std::fs::create_dir_all(root_a.path().join("src")).expect("mkdir src");
    std::fs::write(root_a.path().join("src").join("main.rs"), "fn main() {}\n")
        .expect("write src/main.rs");
    let root_a = root_a.path().canonicalize().expect("canonical root_a");

    // Simulate another connection having recently touched a different root.
    let svc_a = ContextFinderService::new_daemon().clone_for_connection();
    svc_a.state.engine_handle(&root_a).await;

    // New connection with no explicit path should fail-closed, rather than guessing a root
    // from shared (cross-session) recent_roots based on relative file hints.
    let svc_b = svc_a.clone_for_connection();
    let err = svc_b
        .resolve_root_with_hints_no_daemon_touch(None, &["src/main.rs".to_string()])
        .await
        .expect_err("expected daemon to refuse root inference from relative hints");
    assert!(
        err.contains("Missing project root"),
        "expected missing-root error, got: {err}"
    );
}

#[tokio::test]
async fn daemon_refuses_absolute_path_outside_session_root() {
    // Use a git marker to make the temp dir a plausible project root.
    let dir = tempdir().expect("temp dir");
    std::fs::create_dir(dir.path().join(".git")).expect("create .git");
    let canonical_root = dir.path().canonicalize().expect("canonical root");
    let root_display = canonical_root.to_string_lossy().to_string();

    let service = ContextFinderService::new_daemon();
    {
        let mut session = service.session.lock().await;
        session.reset_for_initialize(false);
        session.set_root(
            canonical_root.clone(),
            root_display,
            None,
            RootUpdateSource::RootSet,
            None,
            None,
        );
    }

    let err = service
        .resolve_root_no_daemon_touch(Some("/etc/passwd"))
        .await
        .expect_err("expected absolute out-of-project path to be rejected");
    assert!(
        err.contains("outside the current project"),
        "unexpected error: {err}"
    );

    let (resolved, _) = service
        .resolve_root_no_daemon_touch(None)
        .await
        .expect("root should remain sticky after rejection");
    assert_eq!(resolved, canonical_root);
}

#[tokio::test]
async fn daemon_accepts_absolute_file_hint_within_session_root() {
    let dir = tempdir().expect("temp dir");
    std::fs::create_dir(dir.path().join(".git")).expect("create .git");

    let nested = dir.path().join("sub").join("inner");
    std::fs::create_dir_all(&nested).expect("create nested dir");
    let file = nested.join("main.rs");
    std::fs::write(&file, "fn main() {}\\n").expect("write file");

    let canonical_root = dir.path().canonicalize().expect("canonical root");
    let root_display = canonical_root.to_string_lossy().to_string();
    let canonical_file = file.canonicalize().expect("canonical file");
    let file_str = canonical_file.to_string_lossy().to_string();

    let service = ContextFinderService::new_daemon();
    {
        let mut session = service.session.lock().await;
        session.reset_for_initialize(false);
        session.set_root(
            canonical_root.clone(),
            root_display,
            None,
            RootUpdateSource::RootSet,
            None,
            None,
        );
    }

    let (resolved, _) = service
        .resolve_root_no_daemon_touch(Some(&file_str))
        .await
        .expect("expected absolute file hint within root to be accepted");
    assert_eq!(resolved, canonical_root);

    let focus = service
        .session
        .lock()
        .await
        .focus_file()
        .expect("expected focus file to be set");
    assert_eq!(focus, "sub/inner/main.rs");
}

#[test]
fn canonicalize_root_prefers_git_root_for_file_hint() {
    let dir = tempdir().expect("temp dir");
    std::fs::create_dir(dir.path().join(".git")).expect("create .git");

    let nested = dir.path().join("sub").join("inner");
    std::fs::create_dir_all(&nested).expect("create nested dir");
    let file = nested.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write file");

    let resolved = canonicalize_root_path(&file).expect("canonicalize root");
    assert_eq!(resolved, dir.path().canonicalize().expect("canonical root"));
}

#[test]
fn root_path_from_mcp_uri_parses_file_uri() {
    let out = root_path_from_mcp_uri("file:///tmp/foo%20bar").expect("parse file uri");
    assert_eq!(out, PathBuf::from("/tmp/foo bar"));
}

#[tokio::test]
async fn session_refuses_root_outside_workspace_roots_until_explicit_path() {
    let ws = tempdir().expect("temp workspace");
    let other = tempdir().expect("temp other");
    let ws_root = ws.path().canonicalize().expect("canonical ws_root");
    let other_root = other.path().canonicalize().expect("canonical other_root");

    let service = ContextFinderService::new_daemon();
    {
        let mut session = service.session.lock().await;
        session.reset_for_initialize(false);
        session.set_mcp_workspace_roots(vec![ws_root.clone()]);
        session.set_root(
            other_root.clone(),
            other_root.to_string_lossy().to_string(),
            None,
            RootUpdateSource::RootSet,
            None,
            None,
        );
    }

    let err = service
        .resolve_root_no_daemon_touch(None)
        .await
        .expect_err("expected session root outside workspace to fail");
    assert!(
        err.contains("outside MCP workspace roots"),
        "expected outside-workspace error, got: {err}"
    );

    let (resolved, _) = service
        .resolve_root_no_daemon_touch(Some(&ws_root.to_string_lossy()))
        .await
        .expect("explicit workspace root should resolve");
    assert_eq!(resolved, ws_root);

    let (resolved, _) = service
        .resolve_root_no_daemon_touch(None)
        .await
        .expect("expected session to recover once explicit root is set");
    assert_eq!(resolved, ws_root);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn relative_path_is_resolved_against_session_root_before_process_cwd() {
    // Root resolution must be deterministic for agents: a relative `path` like `README.md` should
    // not jump to the server process cwd (which may point at a different project in shared/daemon
    // setups). Prefer the established session root when available.
    let _lock = ENV_MUTEX.lock().expect("ENV_MUTEX");

    struct CwdGuard {
        saved: std::path::PathBuf,
    }
    impl CwdGuard {
        fn new(path: &std::path::Path) -> Self {
            let saved = std::env::current_dir().expect("current_dir");
            std::env::set_current_dir(path).expect("set_current_dir");
            Self { saved }
        }
    }
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.saved);
        }
    }

    let root_a = tempdir().expect("temp root_a");
    std::fs::create_dir(root_a.path().join(".git")).expect("create .git");
    std::fs::write(root_a.path().join("README.md"), "# A\n").expect("write README");
    let root_a = root_a.path().canonicalize().expect("canonical root_a");

    let root_b = tempdir().expect("temp root_b");
    std::fs::create_dir(root_b.path().join(".git")).expect("create .git");
    std::fs::write(root_b.path().join("README.md"), "# B\n").expect("write README");
    let root_b = root_b.path().canonicalize().expect("canonical root_b");

    let _cwd = CwdGuard::new(&root_b);

    let service = ContextFinderService::new_daemon();
    {
        let mut session = service.session.lock().await;
        session.reset_for_initialize(false);
        session.set_root(
            root_a.clone(),
            root_a.to_string_lossy().to_string(),
            None,
            RootUpdateSource::RootSet,
            None,
            None,
        );
    }

    let (resolved, _) = service
        .resolve_root_no_daemon_touch(Some("README.md"))
        .await
        .expect("root must resolve via session-root-relative file hint");
    assert_eq!(resolved, root_a);
}

#[tokio::test]
async fn daemon_rejects_relative_path_without_session_or_workspace_root() {
    let service = ContextFinderService::new_daemon();
    let err = service
        .resolve_root_no_daemon_touch(Some("README.md"))
        .await
        .expect_err("expected relative path without roots to fail-closed in daemon mode");
    assert!(
        err.contains("relative `path` is ambiguous"),
        "expected fail-closed error, got: {err}"
    );
}

#[tokio::test]
async fn root_tools_can_set_and_report_session_root() {
    let dir = tempdir().expect("temp dir");
    std::fs::create_dir(dir.path().join(".git")).expect("create .git");
    let canonical_root = dir.path().canonicalize().expect("canonical root");
    let root_str = canonical_root.to_string_lossy().to_string();

    let service = ContextFinderService::new_daemon();

    let _ = crate::tools::dispatch::router::root::root_set(
        &service,
        RootSetRequest {
            path: root_str.clone(),
        },
    )
    .await
    .expect("root_set");

    let out = crate::tools::dispatch::router::root::root_get(&service, RootGetRequest::default())
        .await
        .expect("root_get");
    let payload = out.structured_content.expect("structured_content");
    let result: RootGetResult = serde_json::from_value(payload).expect("parse RootGetResult");

    assert_eq!(result.session_root, Some(root_str.clone()));
    assert_eq!(
        result.meta.root_fingerprint,
        Some(context_indexer::root_fingerprint(&root_str))
    );

    let last_root_set = result.last_root_set.expect("last_root_set");
    let last_root_update = result.last_root_update.expect("last_root_update");
    assert_eq!(last_root_set.source, "root_set");
    assert_eq!(last_root_set.requested_path, Some(root_str.clone()));
    assert_eq!(last_root_set.source_tool, Some("root_set".to_string()));
    assert!(last_root_set.at_ms > 0, "expected at_ms to be populated");
    assert_eq!(last_root_update, last_root_set);
}

#[tokio::test]
async fn root_set_can_switch_projects_even_when_session_root_is_already_set() {
    let root_a = tempdir().expect("temp root_a");
    std::fs::create_dir(root_a.path().join(".git")).expect("create .git");
    let root_a = root_a.path().canonicalize().expect("canonical root_a");

    let root_b = tempdir().expect("temp root_b");
    std::fs::create_dir(root_b.path().join(".git")).expect("create .git");
    let root_b = root_b.path().canonicalize().expect("canonical root_b");
    let root_b_str = root_b.to_string_lossy().to_string();

    let service = ContextFinderService::new_daemon();
    {
        let mut session = service.session.lock().await;
        session.reset_for_initialize(false);
        session.set_root(
            root_a,
            "A".to_string(),
            None,
            RootUpdateSource::RootSet,
            None,
            None,
        );
    }

    let _ = crate::tools::dispatch::router::root::root_set(
        &service,
        RootSetRequest {
            path: root_b_str.clone(),
        },
    )
    .await
    .expect("root_set");

    let out = crate::tools::dispatch::router::root::root_get(&service, RootGetRequest::default())
        .await
        .expect("root_get");
    let payload = out.structured_content.expect("structured_content");
    let result: RootGetResult = serde_json::from_value(payload).expect("parse RootGetResult");

    assert_eq!(result.session_root, Some(root_b_str));
}

#[tokio::test]
async fn root_set_disambiguates_multi_root_workspaces() {
    let root_a = tempdir().expect("temp root_a");
    std::fs::create_dir(root_a.path().join(".git")).expect("create .git");
    let root_a = root_a.path().canonicalize().expect("canonical root_a");

    let root_b = tempdir().expect("temp root_b");
    std::fs::create_dir(root_b.path().join(".git")).expect("create .git");
    let root_b = root_b.path().canonicalize().expect("canonical root_b");

    let service = ContextFinderService::new_daemon();
    {
        let mut session = service.session.lock().await;
        session.reset_for_initialize(true);
        session.set_mcp_workspace_roots(vec![root_a.clone(), root_b.clone()]);
        session.set_mcp_roots_ambiguous(true);
    }

    let out = crate::tools::dispatch::router::root::root_get(&service, RootGetRequest::default())
        .await
        .expect("root_get");
    let payload = out.structured_content.expect("structured_content");
    let before: RootGetResult = serde_json::from_value(payload).expect("parse RootGetResult");
    assert!(
        before.workspace_roots_ambiguous,
        "expected ambiguous workspace roots before root_set"
    );

    let _ = crate::tools::dispatch::router::root::root_set(
        &service,
        RootSetRequest {
            path: root_a.to_string_lossy().to_string(),
        },
    )
    .await
    .expect("root_set");

    let out = crate::tools::dispatch::router::root::root_get(&service, RootGetRequest::default())
        .await
        .expect("root_get");
    let payload = out.structured_content.expect("structured_content");
    let after: RootGetResult = serde_json::from_value(payload).expect("parse RootGetResult");
    assert!(
        !after.workspace_roots_ambiguous,
        "expected ambiguity cleared after root_set"
    );
    assert_eq!(
        after.session_root,
        Some(root_a.to_string_lossy().to_string())
    );
}

#[tokio::test]
async fn daemon_can_disambiguate_multi_root_workspace_from_hints() {
    let root_a = tempdir().expect("temp root_a");
    std::fs::create_dir(root_a.path().join(".git")).expect("create .git");
    std::fs::create_dir_all(root_a.path().join("src")).expect("mkdir src");
    std::fs::write(root_a.path().join("src").join("main.rs"), "fn main() {}\n")
        .expect("write src/main.rs");
    let root_a = root_a.path().canonicalize().expect("canonical root_a");

    let root_b = tempdir().expect("temp root_b");
    std::fs::create_dir(root_b.path().join(".git")).expect("create .git");
    let root_b = root_b.path().canonicalize().expect("canonical root_b");

    let service = ContextFinderService::new_daemon();
    {
        let mut session = service.session.lock().await;
        session.reset_for_initialize(true);
        session.set_mcp_workspace_roots(vec![root_a.clone(), root_b.clone()]);
        session.set_mcp_roots_ambiguous(true);
        session.set_roots_pending(false);
    }

    let (resolved, _) = service
        .resolve_root_with_hints_no_daemon_touch(None, &["src/main.rs".to_string()])
        .await
        .expect("expected root to be selected from workspace roots using hint");
    assert_eq!(resolved, root_a);

    let out = crate::tools::dispatch::router::root::root_get(&service, RootGetRequest::default())
        .await
        .expect("root_get");
    let payload = out.structured_content.expect("structured_content");
    let result: RootGetResult = serde_json::from_value(payload).expect("parse RootGetResult");
    assert_eq!(
        result.session_root,
        Some(root_a.to_string_lossy().to_string())
    );
}

#[tokio::test]
async fn invalid_path_errors_include_root_context_details() {
    let dir = tempdir().expect("temp dir");
    std::fs::create_dir(dir.path().join(".git")).expect("create .git");
    let canonical_root = dir.path().canonicalize().expect("canonical root");
    let root_str = canonical_root.to_string_lossy().to_string();
    let outside = tempdir().expect("temp outside");
    let outside_root = outside.path().canonicalize().expect("canonical outside");

    let service = ContextFinderService::new_daemon();
    let _ = crate::tools::dispatch::router::root::root_set(
        &service,
        RootSetRequest {
            path: root_str.clone(),
        },
    )
    .await
    .expect("root_set");

    let out = crate::tools::dispatch::router::ls::ls(
        &service,
        LsRequest {
            path: Some(outside_root.to_string_lossy().to_string()),
            dir: None,
            all: None,
            allow_secrets: None,
            limit: None,
            max_chars: None,
            response_mode: None,
            cursor: None,
        },
    )
    .await
    .expect("ls");

    let payload = out.structured_content.expect("structured_content");
    let error = payload.get("error").expect("error");
    assert_eq!(
        error.get("code").and_then(serde_json::Value::as_str),
        Some("invalid_request")
    );
    let details = error.get("details").expect("details");
    let root_context = details.get("root_context").expect("root_context");
    assert_eq!(
        root_context
            .get("session_root")
            .and_then(serde_json::Value::as_str),
        Some(root_str.as_str())
    );
    let last_root_set = root_context.get("last_root_set").expect("last_root_set");
    assert_eq!(
        last_root_set
            .get("source")
            .and_then(serde_json::Value::as_str),
        Some("root_set")
    );
    assert_eq!(
        last_root_set
            .get("requested_path")
            .and_then(serde_json::Value::as_str),
        Some(root_str.as_str())
    );
    assert_eq!(
        last_root_set
            .get("source_tool")
            .and_then(serde_json::Value::as_str),
        Some("root_set")
    );
}

#[tokio::test]
async fn daemon_can_disambiguate_multi_root_relative_path_when_unique() {
    let root_a = tempdir().expect("temp root_a");
    std::fs::create_dir(root_a.path().join(".git")).expect("create .git");
    std::fs::write(root_a.path().join("README.md"), "# A\n").expect("write README A");
    let root_a = root_a.path().canonicalize().expect("canonical root_a");

    let root_b = tempdir().expect("temp root_b");
    std::fs::create_dir(root_b.path().join(".git")).expect("create .git");
    let root_b = root_b.path().canonicalize().expect("canonical root_b");

    let service = ContextFinderService::new_daemon();
    {
        let mut session = service.session.lock().await;
        session.reset_for_initialize(true);
        session.set_mcp_workspace_roots(vec![root_a.clone(), root_b.clone()]);
        session.set_mcp_roots_ambiguous(true);
        session.set_roots_pending(false);
    }

    let (resolved, _) = service
        .resolve_root_no_daemon_touch(Some("README.md"))
        .await
        .expect("expected relative path to disambiguate within workspace roots");
    assert_eq!(resolved, root_a);
}

#[tokio::test]
async fn session_roots_do_not_leak_between_connections() {
    let root_a = tempdir().expect("temp root_a");
    std::fs::create_dir(root_a.path().join(".git")).expect("create .git");
    let root_a = root_a.path().canonicalize().expect("canonical root_a");
    let root_a_str = root_a.to_string_lossy().to_string();

    let shared = ContextFinderService::new_daemon();
    let conn_a = shared.clone_for_connection();
    let conn_b = shared.clone_for_connection();

    {
        let mut session = conn_a.session.lock().await;
        session.reset_for_initialize(false);
    }
    {
        let mut session = conn_b.session.lock().await;
        session.reset_for_initialize(false);
    }
    let _ = crate::tools::dispatch::router::root::root_set(
        &conn_a,
        RootSetRequest {
            path: root_a_str.clone(),
        },
    )
    .await
    .expect("root_set on conn_a");

    let err = conn_b
        .resolve_root_no_daemon_touch(None)
        .await
        .expect_err("expected conn_b to have no root");
    assert!(
        err.contains("Missing project root"),
        "expected missing-root error, got: {err}"
    );
}
