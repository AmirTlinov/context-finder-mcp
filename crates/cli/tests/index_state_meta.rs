use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use tempfile::tempdir;

#[allow(deprecated)]
fn run_cli_raw(workdir: &std::path::Path, request: &str) -> (bool, Value) {
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
        "#,
    )
    .unwrap();
    temp
}

#[test]
fn responses_include_index_state_and_stale_is_detected() {
    let temp = setup_repo();
    let root = temp.path();

    let index_request = r#"{"action":"index","payload":{"path":"."}}"#;
    let index_response = run_cli(root, index_request);
    assert_eq!(index_response["status"], "ok");
    assert!(
        index_response["meta"]["index_state"].is_object(),
        "index response must include meta.index_state"
    );

    fs::write(
        root.join("src/lib.rs"),
        r#"
        pub fn greet(name: &str) {
            println!("hello {name}");
        }
        "#,
    )
    .unwrap();

    let search_request = r#"{"action":"search","options":{"stale_policy":"warn"},"payload":{"query":"greet","limit":3,"project":"."}}"#;
    let search_response = run_cli(root, search_request);
    assert_eq!(search_response["status"], "ok");

    let state = &search_response["meta"]["index_state"];
    assert!(
        state.is_object(),
        "search response must include meta.index_state"
    );
    assert_eq!(
        state["stale"].as_bool(),
        Some(true),
        "index should be reported stale after file changes"
    );

    let reasons = state["stale_reasons"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        reasons
            .iter()
            .any(|v| v.as_str() == Some("filesystem_changed")),
        "expected filesystem_changed in stale_reasons, got {reasons:?}"
    );

    let hints = search_response["hints"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        hints
            .iter()
            .filter_map(|v| v.get("type").and_then(Value::as_str))
            .any(|t| t == "warn"),
        "expected at least one warn hint when index is stale"
    );
}
