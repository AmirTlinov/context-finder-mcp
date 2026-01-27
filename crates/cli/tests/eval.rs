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
fn eval_search_reports_mrr_and_recall() {
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

    let eval_request =
        r#"{"action":"eval","payload":{"path":".","dataset":"dataset.json","limit":5}}"#;
    let eval_response = run_cli(root, eval_request);
    assert_eq!(eval_response["status"], "ok");

    let mean_recall = eval_response["data"]["runs"][0]["summary"]["mean_recall"]
        .as_f64()
        .unwrap_or(0.0);
    let mean_mrr = eval_response["data"]["runs"][0]["summary"]["mean_mrr"]
        .as_f64()
        .unwrap_or(0.0);
    let mean_overlap = eval_response["data"]["runs"][0]["summary"]["mean_overlap_ratio"]
        .as_f64()
        .unwrap_or(0.0);
    assert!(mean_recall > 0.0);
    assert!(mean_mrr > 0.0);
    assert!(mean_overlap > 0.0);
}
