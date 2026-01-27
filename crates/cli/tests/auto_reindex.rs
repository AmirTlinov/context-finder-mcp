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
fn stale_policy_auto_reindexes_and_finds_new_code() {
    let temp = setup_repo();
    let root = temp.path();

    let index_request = r#"{"action":"index","payload":{"path":"."}}"#;
    let index_response = run_cli(root, index_request);
    assert_eq!(index_response["status"], "ok");

    fs::write(
        root.join("src/lib.rs"),
        r#"
        pub fn greet(name: &str) {
            println!("AUTO_REINDEX_MARKER {name}");
        }

        pub fn brand_new_symbol() {
            greet("world");
        }
        "#,
    )
    .unwrap();

    let search_request = r#"{"action":"search","options":{"stale_policy":"auto","max_reindex_ms":5000},"payload":{"query":"greet","limit":5,"project":"."}}"#;
    let search_response = run_cli(root, search_request);
    assert_eq!(search_response["status"], "ok");

    let results = search_response["data"]["results"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        results.iter().any(|r| {
            r.get("content")
                .and_then(Value::as_str)
                .is_some_and(|c| c.contains("AUTO_REINDEX_MARKER"))
        }),
        "expected search results to include updated code after auto reindex, got {results:?}"
    );

    let reindex = &search_response["meta"]["index_state"]["reindex"];
    assert!(reindex.is_object(), "expected meta.index_state.reindex");
    assert_eq!(reindex["attempted"].as_bool(), Some(true));
    assert_eq!(reindex["performed"].as_bool(), Some(true));
}
