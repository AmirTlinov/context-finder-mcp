use super::super::*;
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
        session.set_root(canonical_root_clone, canonical_display, None);
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
