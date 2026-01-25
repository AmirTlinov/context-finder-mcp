use super::super::ReadPackRequest;

pub(super) fn base_request() -> ReadPackRequest {
    ReadPackRequest {
        path: Some(".".to_string()),
        intent: None,
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
        start_line: None,
        max_lines: None,
        max_chars: None,
        response_mode: None,
        timeout_ms: None,
        cursor: None,
        prefer_code: None,
        include_docs: None,
        allow_secrets: None,
    }
}
