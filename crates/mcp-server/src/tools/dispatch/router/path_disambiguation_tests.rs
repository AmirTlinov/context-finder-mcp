use super::{
    context, context_pack, file_slice, grep_context, list_files, map, meaning_focus, read_pack,
    search, text_search,
};
use crate::tools::dispatch::ContextFinderService;
use crate::tools::schemas::context::ContextRequest;
use crate::tools::schemas::context_pack::ContextPackRequest;
use crate::tools::schemas::file_slice::FileSliceRequest;
use crate::tools::schemas::grep_context::GrepContextRequest;
use crate::tools::schemas::list_files::ListFilesRequest;
use crate::tools::schemas::map::MapRequest;
use crate::tools::schemas::meaning_focus::MeaningFocusRequest;
use crate::tools::schemas::read_pack::{ReadPackIntent, ReadPackRequest};
use crate::tools::schemas::response_mode::ResponseMode;
use crate::tools::schemas::search::SearchRequest;
use crate::tools::schemas::text_search::TextSearchRequest;
use serde_json::json;
use serde_json::Value;
use std::path::Path;
use tempfile::tempdir;

async fn make_daemon_service_with_root(root: &Path) -> ContextFinderService {
    let root = root.canonicalize().expect("canonicalize test root");
    let root_display = root.to_string_lossy().to_string();

    let service = ContextFinderService::new_daemon();
    {
        let mut session = service.session.lock().await;
        session.reset_for_initialize(false);
        session.set_root(root, root_display, None);
    }
    service
}

#[tokio::test]
async fn cat_path_is_treated_as_file_hint_when_session_root_is_set() {
    let dir = tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("subdir")).expect("mkdir subdir");
    std::fs::write(dir.path().join("subdir/hello.txt"), "hello\n").expect("write file");

    let service = make_daemon_service_with_root(dir.path()).await;
    let before_root = {
        service
            .session
            .lock()
            .await
            .clone_root()
            .expect("session root")
            .0
    };

    let request = FileSliceRequest {
        path: Some("subdir/hello.txt".to_string()),
        file: None,
        start_line: Some(1),
        max_lines: Some(50),
        end_line: None,
        max_chars: Some(10_000),
        format: None,
        response_mode: Some(ResponseMode::Full),
        allow_secrets: Some(true),
        cursor: None,
    };

    let output = file_slice::file_slice(&service, &request)
        .await
        .expect("call cat");
    let payload = output
        .structured_content
        .clone()
        .expect("structured_content");
    let file = payload
        .get("file")
        .and_then(Value::as_str)
        .expect("payload.file");
    let content = payload
        .get("content")
        .and_then(Value::as_str)
        .expect("payload.content");
    assert_eq!(file, "subdir/hello.txt");
    assert!(content.contains("hello"));

    let after_root = {
        service
            .session
            .lock()
            .await
            .clone_root()
            .expect("session root")
            .0
    };
    assert_eq!(before_root, after_root);
}

#[tokio::test]
async fn context_pack_path_is_treated_as_scope_hint_when_session_root_is_set() {
    let dir = tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("subdir")).expect("mkdir subdir");
    std::fs::write(dir.path().join("subdir/a.txt"), "foo\n").expect("write file");

    let mut request: ContextPackRequest = serde_json::from_value(json!({
        "query": "foo",
        "path": "subdir",
    }))
    .expect("deserialize request");

    let applied = context_pack::disambiguate_context_pack_path_as_scope_hint_if_root_set(
        Some(dir.path()),
        &mut request,
    );

    assert!(applied);
    assert!(request.path.is_none());
    assert_eq!(
        request
            .include_paths
            .as_ref()
            .expect("include_paths")
            .as_slice(),
        ["subdir"]
    );
}

#[tokio::test]
async fn meaning_focus_path_is_treated_as_focus_prefix_when_session_root_is_set() {
    let dir = tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("subdir")).expect("mkdir subdir");
    std::fs::write(dir.path().join("subdir/hello.txt"), "hello\n").expect("write file");

    let mut request: MeaningFocusRequest = serde_json::from_value(json!({
        "path": "subdir",
        "focus": "hello.txt",
    }))
    .expect("deserialize request");

    let applied = meaning_focus::disambiguate_meaning_focus_path_as_focus_prefix_if_root_set(
        Some(dir.path()),
        &mut request,
    );

    assert!(applied);
    assert!(request.path.is_none());
    assert_eq!(request.focus, "subdir/hello.txt");
}

#[tokio::test]
async fn search_path_is_treated_as_scope_hint_when_session_root_is_set() {
    let dir = tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("subdir")).expect("mkdir subdir");
    std::fs::write(dir.path().join("subdir/a.txt"), "foo\n").expect("write file");

    let mut request: SearchRequest = serde_json::from_value(json!({
        "query": "foo",
        "path": "subdir",
    }))
    .expect("deserialize request");

    let applied =
        search::disambiguate_search_path_as_scope_hint_if_root_set(Some(dir.path()), &mut request);

    assert!(applied);
    assert!(request.path.is_none());
    assert_eq!(
        request
            .include_paths
            .as_ref()
            .expect("include_paths")
            .as_slice(),
        ["subdir"]
    );
}

#[tokio::test]
async fn context_path_is_treated_as_scope_hint_when_session_root_is_set() {
    let dir = tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("subdir")).expect("mkdir subdir");
    std::fs::write(dir.path().join("subdir/a.txt"), "foo\n").expect("write file");

    let mut request: ContextRequest = serde_json::from_value(json!({
        "query": "foo",
        "path": "subdir",
    }))
    .expect("deserialize request");

    let applied = context::disambiguate_context_path_as_scope_hint_if_root_set(
        Some(dir.path()),
        &mut request,
    );

    assert!(applied);
    assert!(request.path.is_none());
    assert_eq!(
        request
            .include_paths
            .as_ref()
            .expect("include_paths")
            .as_slice(),
        ["subdir"]
    );
}

#[tokio::test]
async fn rg_path_is_treated_as_file_pattern_hint_when_session_root_is_set() {
    let dir = tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("subdir")).expect("mkdir subdir");
    std::fs::write(dir.path().join("subdir/a.txt"), "foo bar\n").expect("write file");

    let service = make_daemon_service_with_root(dir.path()).await;
    let before_root = {
        service
            .session
            .lock()
            .await
            .clone_root()
            .expect("session root")
            .0
    };

    let request = GrepContextRequest {
        path: Some("subdir".to_string()),
        pattern: Some("foo".to_string()),
        literal: Some(true),
        file: None,
        file_pattern: None,
        context: None,
        before: None,
        after: None,
        max_matches: Some(100),
        max_hunks: Some(20),
        max_chars: Some(20_000),
        case_sensitive: Some(true),
        format: None,
        response_mode: Some(ResponseMode::Full),
        allow_secrets: Some(true),
        cursor: None,
    };

    let output = grep_context::grep_context(&service, request)
        .await
        .expect("call rg");
    let payload = output
        .structured_content
        .clone()
        .expect("structured_content");
    let file_pattern = payload
        .get("file_pattern")
        .and_then(Value::as_str)
        .expect("payload.file_pattern");
    let hunks = payload
        .get("hunks")
        .and_then(Value::as_array)
        .expect("payload.hunks");

    assert_eq!(file_pattern, "subdir/");
    assert!(hunks.iter().any(|hunk| {
        hunk.get("content")
            .and_then(Value::as_str)
            .map(|content| content.contains("foo"))
            .unwrap_or(false)
    }));

    let after_root = {
        service
            .session
            .lock()
            .await
            .clone_root()
            .expect("session root")
            .0
    };
    assert_eq!(before_root, after_root);
}

#[tokio::test]
async fn find_path_is_treated_as_file_pattern_when_session_root_is_set() {
    let dir = tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("subdir")).expect("mkdir subdir");
    std::fs::write(dir.path().join("subdir/a.txt"), "foo\n").expect("write file");
    std::fs::write(dir.path().join("b.txt"), "foo\n").expect("write file");

    let service = make_daemon_service_with_root(dir.path()).await;
    let before_root = {
        service
            .session
            .lock()
            .await
            .clone_root()
            .expect("session root")
            .0
    };

    let request = ListFilesRequest {
        path: Some("subdir".to_string()),
        file_pattern: None,
        limit: Some(200),
        max_chars: Some(10_000),
        response_mode: Some(ResponseMode::Full),
        allow_secrets: Some(true),
        cursor: None,
    };

    let output = list_files::list_files(&service, request)
        .await
        .expect("call find");
    let payload = output
        .structured_content
        .clone()
        .expect("structured_content");
    let files = payload
        .get("files")
        .and_then(Value::as_array)
        .expect("payload.files");

    assert!(files
        .iter()
        .any(|item| item.as_str() == Some("subdir/a.txt")));

    let after_root = {
        service
            .session
            .lock()
            .await
            .clone_root()
            .expect("session root")
            .0
    };
    assert_eq!(before_root, after_root);
}

#[tokio::test]
async fn text_search_path_is_treated_as_file_pattern_when_session_root_is_set() {
    let dir = tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("subdir")).expect("mkdir subdir");
    std::fs::write(dir.path().join("subdir/a.txt"), "needle\n").expect("write file");
    std::fs::write(dir.path().join("b.txt"), "needle\n").expect("write file");

    let service = make_daemon_service_with_root(dir.path()).await;
    let before_root = {
        service
            .session
            .lock()
            .await
            .clone_root()
            .expect("session root")
            .0
    };

    let request = TextSearchRequest {
        pattern: Some("needle".to_string()),
        path: Some("subdir".to_string()),
        max_chars: Some(20_000),
        file_pattern: None,
        max_results: Some(50),
        case_sensitive: Some(true),
        whole_word: Some(false),
        response_mode: Some(ResponseMode::Full),
        allow_secrets: Some(true),
        cursor: None,
    };

    let output = text_search::text_search(&service, request)
        .await
        .expect("call text_search");
    let payload = output
        .structured_content
        .clone()
        .expect("structured_content");
    let matches = payload
        .get("matches")
        .and_then(Value::as_array)
        .expect("payload.matches");

    assert!(matches.iter().any(|item| {
        item.get("file")
            .and_then(Value::as_str)
            .map(|file| file == "subdir/a.txt")
            .unwrap_or(false)
    }));

    let after_root = {
        service
            .session
            .lock()
            .await
            .clone_root()
            .expect("session root")
            .0
    };
    assert_eq!(before_root, after_root);
}

#[tokio::test]
async fn map_path_is_treated_as_scope_when_session_root_is_set() {
    let dir = tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("subdir/a")).expect("mkdir subdir/a");
    std::fs::create_dir_all(dir.path().join("subdir/b")).expect("mkdir subdir/b");
    std::fs::write(dir.path().join("subdir/a/a.rs"), "fn a() { 1 }\n").expect("write file");
    std::fs::write(dir.path().join("subdir/b/b.rs"), "fn b() { 2 }\n").expect("write file");
    std::fs::write(dir.path().join("outside.rs"), "fn c() { 3 }\n").expect("write file");

    let service = make_daemon_service_with_root(dir.path()).await;
    let before_root = {
        service
            .session
            .lock()
            .await
            .clone_root()
            .expect("session root")
            .0
    };

    let request = MapRequest {
        path: Some("subdir".to_string()),
        depth: Some(2),
        limit: Some(1),
        response_mode: Some(ResponseMode::Full),
        cursor: None,
    };
    let output = map::map(&service, request).await.expect("call map");
    let payload = output
        .structured_content
        .clone()
        .expect("structured_content");
    let dirs = payload
        .get("directories")
        .and_then(Value::as_array)
        .expect("payload.directories");
    assert_eq!(dirs.len(), 1);

    let cursor = payload
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("payload.next_cursor");

    let request2 = MapRequest {
        path: None,
        depth: None,
        limit: None,
        response_mode: Some(ResponseMode::Facts),
        cursor: Some(cursor.to_string()),
    };
    let output2 = map::map(&service, request2).await.expect("call map 2");
    let payload2 = output2
        .structured_content
        .clone()
        .expect("structured_content 2");
    let dirs2 = payload2
        .get("directories")
        .and_then(Value::as_array)
        .expect("payload2.directories");
    assert_eq!(dirs2.len(), 1);

    let mut seen = std::collections::HashSet::new();
    for list in [dirs, dirs2] {
        for item in list {
            let path = item.get("path").and_then(Value::as_str).expect("dir.path");
            assert!(path.starts_with("subdir/"), "unexpected dir path: {path}");
            seen.insert(path.to_string());
        }
    }
    assert_eq!(seen.len(), 2);

    let after_root = {
        service
            .session
            .lock()
            .await
            .clone_root()
            .expect("session root")
            .0
    };
    assert_eq!(before_root, after_root);
}

#[tokio::test]
async fn read_pack_path_is_treated_as_file_or_pattern_hint_when_session_root_is_set() {
    let dir = tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("subdir")).expect("mkdir subdir");
    std::fs::write(dir.path().join("subdir/a.txt"), "needle\n").expect("write file");

    let service = make_daemon_service_with_root(dir.path()).await;
    let before_root = {
        service
            .session
            .lock()
            .await
            .clone_root()
            .expect("session root")
            .0
    };

    // File hint: `path` points at a file, so read_pack should treat it as `file`.
    let request = ReadPackRequest {
        path: Some("subdir/a.txt".to_string()),
        intent: Some(ReadPackIntent::File),
        file: None,
        pattern: None,
        query: None,
        ask: None,
        questions: None,
        topics: None,
        file_pattern: None,
        include_paths: None,
        exclude_paths: None,
        before: None,
        after: None,
        case_sensitive: None,
        start_line: Some(1),
        max_lines: Some(50),
        max_chars: Some(10_000),
        response_mode: Some(ResponseMode::Facts),
        allow_secrets: Some(true),
        timeout_ms: Some(5_000),
        cursor: None,
        prefer_code: None,
        include_docs: None,
    };
    let output = read_pack::read_pack(&service, request)
        .await
        .expect("call read_pack file hint");
    let payload = output
        .structured_content
        .clone()
        .expect("structured_content");
    let sections = payload
        .get("sections")
        .and_then(Value::as_array)
        .expect("payload.sections");
    let has_file_snippet = sections.iter().any(|section| {
        section.get("type").and_then(Value::as_str) == Some("snippet")
            && section
                .get("result")
                .and_then(|r| r.get("file"))
                .and_then(Value::as_str)
                == Some("subdir/a.txt")
    });
    assert!(has_file_snippet);

    // Directory hint: `path` points at a directory; when combined with `pattern`, read_pack should
    // treat it as `file_pattern` and keep the session root stable.
    let request2 = ReadPackRequest {
        path: Some("subdir".to_string()),
        intent: Some(ReadPackIntent::Grep),
        file: None,
        pattern: Some("needle".to_string()),
        query: None,
        ask: None,
        questions: None,
        topics: None,
        file_pattern: None,
        include_paths: None,
        exclude_paths: None,
        before: None,
        after: None,
        case_sensitive: Some(true),
        start_line: None,
        max_lines: None,
        max_chars: Some(20_000),
        response_mode: Some(ResponseMode::Full),
        allow_secrets: Some(true),
        timeout_ms: Some(5_000),
        cursor: None,
        prefer_code: None,
        include_docs: None,
    };
    let output2 = read_pack::read_pack(&service, request2)
        .await
        .expect("call read_pack dir hint");
    let payload2 = output2
        .structured_content
        .clone()
        .expect("structured_content 2");
    let sections2 = payload2
        .get("sections")
        .and_then(Value::as_array)
        .expect("payload2.sections");
    let has_grep_section = sections2.iter().any(|section| {
        section.get("type").and_then(Value::as_str) == Some("grep_context")
            && section
                .get("result")
                .and_then(|r| r.get("file_pattern"))
                .and_then(Value::as_str)
                == Some("subdir/")
    });
    assert!(
        has_grep_section,
        "unexpected read_pack grep payload: {payload2:#?}"
    );

    let after_root = {
        service
            .session
            .lock()
            .await
            .clone_root()
            .expect("session root")
            .0
    };
    assert_eq!(before_root, after_root);
}
