use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

#[allow(deprecated)]
fn run_cli_raw(workdir: &Path, request: &str) -> (bool, Value) {
    let output = Command::cargo_bin("context")
        .expect("binary")
        .current_dir(workdir)
        .env("CONTEXT_EMBEDDING_MODE", "stub")
        .arg("command")
        .arg("--json")
        .arg(request)
        .output()
        .expect("command run");

    let body: Value = serde_json::from_slice(&output.stdout).expect("valid json");
    (output.status.success(), body)
}

fn run_cli(workdir: &Path, request: &str) -> Value {
    let (ok, body) = run_cli_raw(workdir, request);
    assert!(ok, "stdout: {body}\nstderr: {request}");
    body
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
fn eval_compare_reports_deltas() {
    let temp = setup_repo();
    let root = temp.path();

    fs::write(
        root.join("dataset.json"),
        r#"
        {
          "schema_version": 1,
          "name": "smoke",
          "cases": [
            {
              "id": "path_query",
              "query": "src/lib.rs",
              "expected_paths": ["src/lib.rs"]
            }
          ]
        }
        "#,
    )
    .unwrap();

    let index_request = r#"{"action":"index","payload":{"path":"."}}"#;
    let index_response = run_cli(root, index_request);
    assert_eq!(index_response["status"], "ok");

    let compare_request = r#"
        {
          "action": "eval_compare",
          "payload": {
            "path": ".",
            "dataset": "dataset.json",
            "limit": 5,
            "a": { "profile": "general" },
            "b": { "profile": "general" }
          }
        }
    "#;
    let compare_response = run_cli(root, compare_request);
    assert_eq!(compare_response["status"], "ok");
    assert!(compare_response["data"]["summary"]["delta_mean_mrr"].is_number());
    let cases = compare_response["data"]["cases"]
        .as_array()
        .expect("cases array");
    assert_eq!(cases.len(), 1);
}
