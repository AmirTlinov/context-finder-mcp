use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use std::fs;
use tempfile::tempdir;

fn run_cli_raw(workdir: &std::path::Path, request: &str) -> (bool, Value) {
    let output = cargo_bin_cmd!("context-finder")
        .current_dir(workdir)
        .env("CONTEXT_FINDER_EMBEDDING_MODE", "stub")
        .arg("command")
        .arg("--json")
        .arg(request)
        .output()
        .expect("command run");

    let body: Value = serde_json::from_slice(&output.stdout).expect("valid json");
    (output.status.success(), body)
}

fn setup_repo() -> tempfile::TempDir {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
        pub fn greet(name: &str) {
            println!("hi {name}");
        }
        "#,
    )
    .unwrap();
    temp
}

#[test]
fn text_search_falls_back_to_filesystem_by_default() {
    let temp = setup_repo();
    let root = temp.path();

    let req = r#"{"action":"text_search","payload":{"pattern":"greet","project":"."}}"#;
    let (ok, resp) = run_cli_raw(root, req);
    assert!(ok, "expected ok, got {resp}");
    assert_eq!(resp["status"], "ok");
    assert_eq!(resp["data"]["source"], "filesystem");
    let matches = resp["data"]["matches"].as_array().expect("matches array");
    assert!(!matches.is_empty(), "expected at least one match");
}

#[test]
fn text_search_respects_allow_filesystem_fallback_flag() {
    let temp = setup_repo();
    let root = temp.path();

    let req = r#"{"action":"text_search","options":{"allow_filesystem_fallback":false},"payload":{"pattern":"greet","project":"."}}"#;
    let (ok, resp) = run_cli_raw(root, req);
    assert!(!ok, "expected non-zero exit due to error");
    assert_eq!(resp["status"], "error");
    let message = resp["message"].as_str().unwrap_or_default().to_string();
    assert!(
        message.contains("filesystem fallback is disabled"),
        "unexpected message: {message}"
    );
}

#[test]
fn text_search_uses_env_root_when_project_missing() {
    let repo = setup_repo();
    let root = repo.path();
    let workdir = tempdir().unwrap();

    let request = r#"{"action":"text_search","payload":{"pattern":"greet"}}"#;
    let output = cargo_bin_cmd!("context-finder")
        .current_dir(workdir.path())
        .env("CONTEXT_FINDER_EMBEDDING_MODE", "stub")
        .env("CONTEXT_FINDER_ROOT", root)
        .arg("command")
        .arg("--json")
        .arg(request)
        .output()
        .expect("command run");

    let body: Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert!(output.status.success(), "expected ok, got {body}");
    let matches = body["data"]["matches"].as_array().expect("matches array");
    assert!(
        matches.iter().any(|m| m["file"] == "src/lib.rs"),
        "expected src/lib.rs in matches"
    );
}
