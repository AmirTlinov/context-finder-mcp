use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use tempfile::tempdir;

#[allow(deprecated)]
fn run_cli_raw(workdir: &std::path::Path, request: &str) -> (bool, Value) {
    let output = Command::cargo_bin("context-finder")
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

fn run_cli(workdir: &std::path::Path, request: &str) -> Value {
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

        pub fn hello() {
            greet(\"world\");
        }
        "#,
    )
    .unwrap();
    temp
}

#[test]
fn search_with_context_supports_show_graph_flag() {
    let temp = setup_repo();
    let root = temp.path();

    let index_request = r#"{"action":"index","payload":{"path":"."}}"#;
    let index_response = run_cli(root, index_request);
    assert_eq!(index_response["status"], "ok");

    let search_request = r#"{"action":"search_with_context","payload":{"query":"hello","limit":5,"project":".","show_graph":true,"strategy":"extended"}}"#;
    let response = run_cli(root, search_request);
    assert_eq!(response["status"], "ok");

    let results = response["data"]["results"]
        .as_array()
        .expect("results array");
    assert!(!results.is_empty(), "expected some results");

    // Graph cache meta must be present even if graph miss/hit varies
    assert!(
        response["meta"]["graph_cache"].is_boolean(),
        "graph_cache meta flag must be present"
    );
}

#[test]
fn search_with_context_accepts_deep_strategy_without_graph_output() {
    let temp = setup_repo();
    let root = temp.path();

    let index_request = r#"{"action":"index","payload":{"path":"."}}"#;
    let index_response = run_cli(root, index_request);
    assert_eq!(index_response["status"], "ok");

    let search_request = r#"{"action":"search_with_context","payload":{"query":"greet","limit":5,"project":".","show_graph":false,"strategy":"deep"}}"#;
    let response = run_cli(root, search_request);
    assert_eq!(response["status"], "ok");

    let results = response["data"]["results"]
        .as_array()
        .expect("results array");
    assert!(!results.is_empty(), "expected some results");
    // With show_graph=false we should not emit graph field
    if let Some(first) = results.first().and_then(Value::as_object) {
        assert!(
            !first.contains_key("graph"),
            "graph field should be absent when show_graph=false"
        );
    }
}

#[test]
fn search_rejects_empty_query() {
    let temp = setup_repo();
    let root = temp.path();

    let index_request = r#"{"action":"index","payload":{"path":"."}}"#;
    let index_response = run_cli(root, index_request);
    assert_eq!(index_response["status"], "ok");

    let search_request = r#"{"action":"search","payload":{"query":"   ","limit":5,"project":"."}}"#;
    let (_, response) = run_cli_raw(root, search_request);
    assert_eq!(response["status"], "error");
    let error_text = response["message"]
        .as_str()
        .or_else(|| response["error"].as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(error_text.contains("empty"), "should mention empty query");
}
