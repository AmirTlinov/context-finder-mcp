use assert_cmd::Command;
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
    assert!(ok, "stdout: {body}\nrequest: {request}");
    body
}

fn setup_repo() -> tempfile::TempDir {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
        pub fn greet_user(name: &str) {
            helper(name);
        }

        fn helper(name: &str) {
            println!("hi {name}");
        }
        "#,
    )
    .unwrap();
    temp
}

#[test]
fn graph_nodes_is_enabled_for_conceptual_queries_only() {
    let temp = setup_repo();
    let root = temp.path();

    let index_request = r#"{"action":"index","payload":{"path":"."}}"#;
    let index_response = run_cli(root, index_request);
    assert_eq!(index_response["status"], "ok");

    let conceptual = r#"{"action":"context_pack","payload":{"project":".","query":"how does greet_user work","strategy":"extended","limit":3,"max_chars":8000}}"#;
    let conceptual_response = run_cli(root, conceptual);
    assert_eq!(conceptual_response["status"], "ok");
    let conceptual_hints = conceptual_response["hints"].as_array().unwrap();
    assert!(
        conceptual_hints.iter().any(|h| {
            h["text"]
                .as_str()
                .unwrap_or_default()
                .starts_with("graph_nodes:")
        }),
        "expected graph_nodes hint, got: {conceptual_hints:?}"
    );

    let identifier = r#"{"action":"context_pack","payload":{"project":".","query":"greet_user","strategy":"extended","limit":3,"max_chars":8000}}"#;
    let identifier_response = run_cli(root, identifier);
    assert_eq!(identifier_response["status"], "ok");
    let identifier_hints = identifier_response["hints"].as_array().unwrap();
    assert!(
        !identifier_hints.iter().any(|h| {
            h["text"]
                .as_str()
                .unwrap_or_default()
                .starts_with("graph_nodes:")
        }),
        "did not expect graph_nodes hint, got: {identifier_hints:?}"
    );
}
