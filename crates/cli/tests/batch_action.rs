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
        "#,
    )
    .unwrap();
    temp
}

#[test]
fn batch_allows_index_then_search_with_stale_policy_fail() {
    let temp = setup_repo();
    let root = temp.path();

    let request = r#"{
        "action":"batch",
        "options":{"stale_policy":"fail"},
        "payload":{
            "project":".",
            "items":[
                {"id":"index","action":"index","payload":{}},
                {"id":"search","action":"search","payload":{"query":"greet","limit":5}}
            ]
        }
    }"#;

    let response = run_cli(root, request);
    assert_eq!(response["status"], "ok");

    let items = response["data"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let search = items
        .iter()
        .find(|item| item["id"].as_str() == Some("search"))
        .expect("search item");
    assert_eq!(search["status"], "ok");

    let results = search["data"]["results"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !results.is_empty(),
        "expected search results, got {results:?}"
    );

    let state = &search["meta"]["index_state"];
    assert!(state.is_object(), "expected meta.index_state, got {state}");
    assert_eq!(state["stale"].as_bool(), Some(false));
}

#[test]
fn batch_returns_item_error_without_failing_whole_response() {
    let temp = setup_repo();
    let root = temp.path();

    let request = r#"{
        "action":"batch",
        "options":{"stale_policy":"auto","max_reindex_ms":5000},
        "payload":{
            "project":".",
            "items":[
                {"id":"index","action":"index","payload":{}},
                {"id":"ok","action":"search","payload":{"query":"greet","limit":3}},
                {"id":"bad","action":"search","payload":{}}
            ]
        }
    }"#;

    let response = run_cli(root, request);
    assert_eq!(response["status"], "ok");

    let items = response["data"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let ok = items
        .iter()
        .find(|item| item["id"].as_str() == Some("ok"))
        .expect("ok item");
    assert_eq!(ok["status"], "ok");

    let bad = items
        .iter()
        .find(|item| item["id"].as_str() == Some("bad"))
        .expect("bad item");
    assert_eq!(bad["status"], "error");
    assert!(!bad["message"].as_str().unwrap_or_default().is_empty());
}

#[test]
fn batch_respects_max_chars_and_truncates() {
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

    let mut big = String::new();
    for _ in 0..5000 {
        big.push_str("// repeat_me\n");
    }
    fs::write(root.join("src/big.rs"), big).unwrap();

    let request = r#"{
        "action":"batch",
        "payload":{
            "project":".",
            "max_chars":1500,
            "items":[
                {"id":"index","action":"index","payload":{}},
                {"id":"huge","action":"text_search","payload":{"pattern":"repeat_me","max_results":1000}}
            ]
        }
    }"#;

    let response = run_cli(root, request);
    assert_eq!(response["status"], "ok");

    let budget = &response["data"]["budget"];
    assert_eq!(budget["max_chars"].as_u64(), Some(1500));
    assert_eq!(budget["truncated"].as_bool(), Some(true));
    assert!(budget["used_chars"].as_u64().unwrap_or(0) <= 1500);

    let items = response["data"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(items.len(), 1, "expected only the first item to fit");
    assert_eq!(items[0]["id"], "index");
}

#[test]
fn batch_resolves_refs_between_items() {
    let temp = setup_repo();
    let root = temp.path();

    let request = r##"{
        "action":"batch",
        "payload":{
            "project":".",
            "items":[
                {"id":"index","action":"index","payload":{}},
                {"id":"search","action":"text_search","payload":{"pattern":"greet","max_results":1}},
                {"id":"ctx","action":"get_context","payload":{
                    "file": { "$ref": "#/items/search/data/matches/0/file" },
                    "line": { "$ref": "#/items/search/data/matches/0/line" },
                    "window": 0
                }}
            ]
        }
    }"##;

    let response = run_cli(root, request);
    assert_eq!(response["status"], "ok");

    let items = response["data"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let search = items
        .iter()
        .find(|item| item["id"].as_str() == Some("search"))
        .expect("search item");
    assert_eq!(search["status"], "ok");

    let ctx_item = items
        .iter()
        .find(|item| item["id"].as_str() == Some("ctx"))
        .expect("ctx item");
    assert_eq!(ctx_item["status"], "ok");

    assert_eq!(
        ctx_item["data"]["file"],
        search["data"]["matches"][0]["file"]
    );
    assert_eq!(
        ctx_item["data"]["line"],
        search["data"]["matches"][0]["line"]
    );
}
