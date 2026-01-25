use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use tempfile::tempdir;

#[allow(deprecated)]
fn run_cli(workdir: &std::path::Path, request: &str) -> Value {
    let output = Command::cargo_bin("context-finder")
        .expect("binary")
        .current_dir(workdir)
        .env("CONTEXT_EMBEDDING_MODE", "stub")
        .arg("command")
        .arg("--json")
        .arg(request)
        .output()
        .expect("command run");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_slice(&output.stdout).expect("valid json")
}

#[test]
fn compare_search_returns_baseline_and_context() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
        pub fn hello() {
            println!("hello");
        }

        pub fn greet(name: &str) {
            println!("hi {}", name);
        }
        "#,
    )
    .unwrap();

    let index_request = r#"{"action":"index","payload":{"path":"."}}"#;
    let index_response = run_cli(root, index_request);
    assert_eq!(index_response["status"], "ok");

    let compare_request = r#"{"action":"compare_search","payload":{"queries":["hello"],"limit":5,"invalidate_cache":true}}"#;
    let compare_response = run_cli(root, compare_request);
    assert_eq!(compare_response["status"], "ok");

    let data = compare_response["data"].as_object().expect("data object");
    let queries = data["queries"].as_array().expect("queries array");
    assert_eq!(queries.len(), 1);
    let row = &queries[0];
    assert!(!row["baseline"].as_array().unwrap().is_empty());
    assert!(!row["context"].as_array().unwrap().is_empty());
    assert!(data["summary"]["avg_context_ms"].as_f64().unwrap() >= 0.0);
}
