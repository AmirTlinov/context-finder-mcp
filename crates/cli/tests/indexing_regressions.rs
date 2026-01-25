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
fn index_creates_indexes_for_requested_models() {
    let temp = setup_repo();
    let root = temp.path();

    let index_request =
        r#"{"action":"index","payload":{"path":".","models":["bge-base"],"experts":false}}"#;
    let index_response = run_cli(root, index_request);
    assert_eq!(index_response["status"], "ok");

    let context_dir = context_dir_for_project_root(root);
    assert!(
        context_dir
            .join("indexes")
            .join("bge-small")
            .join("index.json")
            .exists(),
        "default model index should exist"
    );
    assert!(
        context_dir
            .join("indexes")
            .join("bge-base")
            .join("index.json")
            .exists(),
        "requested model index should exist"
    );
}

#[test]
fn incremental_index_purges_deleted_files() {
    let temp = setup_repo();
    let root = temp.path();

    fs::write(
        root.join("src/dead.rs"),
        r"
        pub fn dead() -> i32 { 1 }
        ",
    )
    .unwrap();

    let index_request = r#"{"action":"index","payload":{"path":"."}}"#;
    let index_response = run_cli(root, index_request);
    assert_eq!(index_response["status"], "ok");

    fs::remove_file(root.join("src/dead.rs")).unwrap();

    let index_response = run_cli(root, index_request);
    assert_eq!(index_response["status"], "ok");

    let context_dir = context_dir_for_project_root(root);
    let index_path = context_dir
        .join("indexes")
        .join("bge-small")
        .join("index.json");
    let raw = fs::read_to_string(index_path).unwrap();
    let parsed: Value = serde_json::from_str(&raw).unwrap();
    let id_map = parsed["id_map"].as_object().expect("id_map map");

    for chunk_id in id_map.values() {
        let chunk_id = chunk_id.as_str().unwrap_or_default();
        assert!(
            !chunk_id.starts_with("src/dead.rs:"),
            "stale chunk_id was not purged"
        );
    }

    let corpus_path = context_dir.join("corpus.json");
    assert!(corpus_path.exists(), "chunk corpus should exist");
    let corpus_raw = fs::read_to_string(corpus_path).unwrap();
    let corpus: Value = serde_json::from_str(&corpus_raw).unwrap();
    let files = corpus["files"].as_object().expect("corpus files map");
    assert!(
        !files.contains_key("src/dead.rs"),
        "stale corpus file entry was not purged"
    );
}
