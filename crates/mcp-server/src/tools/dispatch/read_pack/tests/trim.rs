use super::super::super::router::cursor_alias::expand_cursor_alias;
use super::super::budget_trim::finalize_and_trim;
use super::super::context::build_context;
use super::super::cursors::ReadPackRecallCursorV1;
use super::super::recall_trim::repair_recall_cursor_after_trim;
use super::super::{
    decode_cursor, ContextFinderService, ProjectFactsResult, ReadPackBudget, ReadPackIntent,
    ReadPackRecallResult, ReadPackResult, ReadPackSection, ReadPackSnippet, ReadPackTruncation,
    ResponseMode,
};
use super::support::base_request;
use std::path::PathBuf;

#[test]
fn cursor_pagination_marks_budget_truncated_even_under_max_chars() {
    let mut request = base_request();
    request.max_chars = Some(2_000);
    let ctx = build_context(&request, PathBuf::from("."), ".".to_string())
        .unwrap_or_else(|_| panic!("build_context should succeed"));

    let result = ReadPackResult {
        version: 1,
        intent: ReadPackIntent::Memory,
        root: ".".to_string(),
        sections: vec![ReadPackSection::ProjectFacts {
            result: ProjectFactsResult {
                version: 1,
                ecosystems: Vec::new(),
                build_tools: Vec::new(),
                ci: Vec::new(),
                contracts: Vec::new(),
                key_dirs: Vec::new(),
                modules: Vec::new(),
                entry_points: Vec::new(),
                key_configs: Vec::new(),
            },
        }],
        next_actions: Vec::new(),
        next_cursor: Some("cfcs1:AAAAAAAAAA".to_string()),
        budget: ReadPackBudget {
            max_chars: ctx.max_chars,
            used_chars: 0,
            truncated: false,
            truncation: None,
        },
        meta: None,
    };

    let result = finalize_and_trim(
        result,
        &ctx,
        &request,
        ReadPackIntent::Memory,
        ResponseMode::Facts,
    )
    .unwrap_or_else(|_| panic!("finalize_and_trim should succeed"));

    assert!(result.budget.truncated);
    assert_eq!(result.budget.truncation, Some(ReadPackTruncation::MaxItems));
}

#[tokio::test]
async fn recall_cursor_repair_overwrites_existing_cursor() {
    let service = ContextFinderService::new();

    let temp = tempfile::tempdir().unwrap();
    let root_display = temp.path().to_string_lossy().to_string();

    let mut request = base_request();
    request.path = Some(root_display.clone());
    request.max_chars = Some(6_000);
    request.questions = Some(vec![
        "Q1: identity".to_string(),
        "Q2: entrypoints".to_string(),
        "Q3: commands".to_string(),
    ]);

    let ctx = build_context(&request, temp.path().to_path_buf(), root_display.clone()).unwrap();

    let mut result = ReadPackResult {
        version: 1,
        intent: ReadPackIntent::Recall,
        root: root_display.clone(),
        sections: vec![
            ReadPackSection::ProjectFacts {
                result: ProjectFactsResult {
                    version: 1,
                    ecosystems: Vec::new(),
                    build_tools: Vec::new(),
                    ci: Vec::new(),
                    contracts: Vec::new(),
                    key_dirs: Vec::new(),
                    modules: Vec::new(),
                    entry_points: Vec::new(),
                    key_configs: Vec::new(),
                },
            },
            ReadPackSection::Recall {
                result: ReadPackRecallResult {
                    question: "Q1: identity".to_string(),
                    snippets: Vec::new(),
                },
            },
        ],
        next_actions: Vec::new(),
        next_cursor: Some("cfcs1:AAAAAAAAAA".to_string()),
        budget: ReadPackBudget {
            max_chars: ctx.max_chars,
            used_chars: 0,
            truncated: true,
            truncation: Some(ReadPackTruncation::MaxChars),
        },
        meta: None,
    };

    repair_recall_cursor_after_trim(&service, &ctx, &request, ResponseMode::Facts, &mut result)
        .await;

    let cursor = result.next_cursor.as_deref().expect("expected next_cursor");
    let expanded = expand_cursor_alias(&service, cursor)
        .await
        .expect("cursor alias should expand in tests");
    let decoded: ReadPackRecallCursorV1 = decode_cursor(&expanded).expect("cursor should decode");

    assert_eq!(
        decoded.questions,
        vec!["Q2: entrypoints".to_string(), "Q3: commands".to_string()]
    );
}

#[test]
fn finalize_and_trim_recall_prefers_dropping_snippets_over_questions() {
    let mut request = base_request();
    request.max_chars = Some(3_000);
    let ctx = build_context(&request, PathBuf::from("."), ".".to_string()).unwrap();

    let big = "x".repeat(1_600);
    let result = ReadPackResult {
        version: 1,
        intent: ReadPackIntent::Recall,
        root: ".".to_string(),
        sections: vec![
            ReadPackSection::ProjectFacts {
                result: ProjectFactsResult {
                    version: 1,
                    ecosystems: Vec::new(),
                    build_tools: Vec::new(),
                    ci: Vec::new(),
                    contracts: Vec::new(),
                    key_dirs: Vec::new(),
                    modules: Vec::new(),
                    entry_points: Vec::new(),
                    key_configs: Vec::new(),
                },
            },
            ReadPackSection::Recall {
                result: ReadPackRecallResult {
                    question: "Q1".to_string(),
                    snippets: vec![
                        ReadPackSnippet {
                            file: "README.md".to_string(),
                            start_line: 1,
                            end_line: 10,
                            content: big.clone(),
                            kind: None,
                            reason: None,
                            next_cursor: None,
                        },
                        ReadPackSnippet {
                            file: "DEVELOPMENT.md".to_string(),
                            start_line: 1,
                            end_line: 10,
                            content: big.clone(),
                            kind: None,
                            reason: None,
                            next_cursor: None,
                        },
                        ReadPackSnippet {
                            file: "Cargo.toml".to_string(),
                            start_line: 1,
                            end_line: 10,
                            content: big,
                            kind: None,
                            reason: None,
                            next_cursor: None,
                        },
                    ],
                },
            },
        ],
        next_actions: Vec::new(),
        next_cursor: None,
        budget: ReadPackBudget {
            max_chars: ctx.max_chars,
            used_chars: 0,
            truncated: false,
            truncation: None,
        },
        meta: None,
    };

    let trimmed = finalize_and_trim(
        result,
        &ctx,
        &request,
        ReadPackIntent::Recall,
        ResponseMode::Facts,
    )
    .unwrap();

    let recall = trimmed
        .sections
        .iter()
        .find_map(|section| match section {
            ReadPackSection::Recall { result } => Some(result),
            _ => None,
        })
        .expect("expected recall section to survive trimming");

    assert!(
        recall.snippets.len() < 3,
        "expected recall trimming to drop snippets before dropping the question"
    );
    assert!(
        !recall.snippets.is_empty(),
        "expected at least one snippet to remain for the question"
    );
}
