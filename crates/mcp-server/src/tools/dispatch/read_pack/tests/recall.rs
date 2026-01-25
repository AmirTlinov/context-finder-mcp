use super::super::intent_recall::{
    best_keyword_pattern, parse_recall_question_directives, recall_question_policy,
    snippets_from_grep_filtered, GrepSnippetParams, RecallQuestionMode,
};
use super::super::{
    build_context, handle_recall_intent, ContextFinderService, ReadPackSection, ResponseMode,
};
use super::support::base_request;
use tempfile::tempdir;

#[test]
fn recall_question_directives_support_fast_deep_and_scoping() {
    let temp = tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("src").join("main.rs"), b"fn main() {}\n").unwrap();

    let (cleaned, directives) =
        parse_recall_question_directives("deep k:5 ctx:4 in:src lit: fn main()", temp.path());

    assert_eq!(directives.mode, RecallQuestionMode::Deep);
    assert_eq!(directives.snippet_limit, Some(5));
    assert_eq!(directives.grep_context, Some(4));
    assert_eq!(directives.include_paths, vec!["src".to_string()]);
    assert_eq!(cleaned, "lit: fn main()".to_string());

    let (cleaned, directives) =
        parse_recall_question_directives("fast not:src lit: cargo test", temp.path());
    assert_eq!(directives.mode, RecallQuestionMode::Fast);
    assert_eq!(directives.exclude_paths, vec!["src".to_string()]);
    assert_eq!(cleaned, "lit: cargo test".to_string());

    let (_cleaned, directives) =
        parse_recall_question_directives("index:5s lit: cursor", temp.path());
    assert_eq!(directives.mode, RecallQuestionMode::Deep);
}

#[test]
fn recall_policy_respects_fast_deep_and_freshness() {
    let policy = recall_question_policy(RecallQuestionMode::Fast, false);
    assert!(!policy.allow_semantic);

    let policy = recall_question_policy(RecallQuestionMode::Auto, false);
    assert!(!policy.allow_semantic);

    let policy = recall_question_policy(RecallQuestionMode::Auto, true);
    assert!(policy.allow_semantic);

    let policy = recall_question_policy(RecallQuestionMode::Deep, false);
    assert!(policy.allow_semantic);
}

#[tokio::test]
async fn recall_upgrades_doc_only_matches_to_code_when_possible() {
    let service = ContextFinderService::new();

    let temp = tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("README.md"), b"velocity docs mention\n").unwrap();
    std::fs::write(root.join("src").join("main.rs"), b"fn velocity() {}\n").unwrap();

    let root_display = root.to_string_lossy().to_string();
    let mut request = base_request();
    request.path = Some(root_display.clone());
    request.questions = Some(vec!["where is velocity computed".to_string()]);
    request.max_chars = Some(1_200);
    request.response_mode = Some(ResponseMode::Facts);

    let ctx = build_context(&request, root.to_path_buf(), root_display.clone()).unwrap();

    let keyword = best_keyword_pattern("where is velocity computed")
        .expect("expected keyword extraction to succeed");
    let (direct_snippets, _) = snippets_from_grep_filtered(
        &ctx,
        &keyword,
        GrepSnippetParams {
            file: None,
            file_pattern: None,
            before: 12,
            after: 12,
            max_hunks: 1,
            max_chars: 900,
            case_sensitive: false,
            allow_secrets: false,
        },
        &[],
        &[],
        None,
    )
    .await
    .unwrap();
    assert!(
        !direct_snippets.is_empty(),
        "expected direct grep fallback to find velocity"
    );
    let mut sections = Vec::new();
    let mut next_cursor = None;

    handle_recall_intent(
        &service,
        &ctx,
        &request,
        ResponseMode::Facts,
        false,
        &mut sections,
        &mut next_cursor,
    )
    .await
    .unwrap();

    let recall = sections.iter().find_map(|section| match section {
        ReadPackSection::Recall { result } => Some(result),
        _ => None,
    });
    let recall = recall.expect("expected recall section");
    assert_eq!(
        recall.snippets.len(),
        1,
        "expected a single snippet under budget"
    );
    assert_eq!(
        recall.snippets[0].file, "src/main.rs",
        "expected recall to prefer code over README matches"
    );
}

// Intentionally focused on recall behavior.
