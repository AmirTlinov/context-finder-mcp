use assert_cmd::Command;
use context_vector_store::context_dir_for_project_root;
use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

#[allow(deprecated)]
fn run_cli_raw(workdir: &Path, request: &str) -> (bool, Value) {
    let output = Command::cargo_bin("context-finder")
        .expect("binary")
        .current_dir(workdir)
        .env("CONTEXT_EMBEDDING_MODE", "stub")
        .env("CONTEXT_PROFILE", "bad")
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

    let context_dir = context_dir_for_project_root(root);
    fs::create_dir_all(context_dir.join("profiles")).unwrap();
    fs::write(
        context_dir.join("profiles/bad.json"),
        r#"
        {
          "schema_version": 1,
          "embedding": {
            "query": {
              "default": "Query: {text}",
              "oops": "broken"
            }
          }
        }
        "#,
    )
    .unwrap();
    temp
}

#[test]
fn cli_profile_validation_reports_unknown_fields_with_paths() {
    let temp = setup_repo();
    let root = temp.path();

    let index_request = r#"{"action":"index","payload":{"path":"."}}"#;
    let (ok, body) = run_cli_raw(root, index_request);
    assert!(!ok, "expected non-zero exit for invalid profile");
    assert_eq!(body["status"], "error");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("embedding.query.oops"),
        "message did not contain the offending path: {message}"
    );
}
