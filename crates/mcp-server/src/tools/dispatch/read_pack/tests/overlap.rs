use super::super::{
    overlap_dedupe_snippet_sections, strip_snippet_reasons_for_output, ProjectFactsResult,
    ReadPackSection, ReadPackSnippet, ReadPackSnippetKind, REASON_ANCHOR_DOC,
    REASON_ANCHOR_FOCUS_FILE, REASON_NEEDLE_FILE_SLICE,
};

#[test]
fn overlap_dedupe_removes_contained_snippet_spans() {
    let mut sections = vec![
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
        ReadPackSection::Snippet {
            result: ReadPackSnippet {
                file: "src/lib.rs".to_string(),
                start_line: 1,
                end_line: 80,
                content: "fn a() {}\n".to_string(),
                kind: Some(ReadPackSnippetKind::Code),
                reason: Some(REASON_NEEDLE_FILE_SLICE.to_string()),
                next_cursor: None,
            },
        },
        ReadPackSection::Snippet {
            result: ReadPackSnippet {
                file: "src/lib.rs".to_string(),
                start_line: 10,
                end_line: 30,
                content: "fn b() {}\n".to_string(),
                kind: Some(ReadPackSnippetKind::Code),
                reason: Some(REASON_NEEDLE_FILE_SLICE.to_string()),
                next_cursor: None,
            },
        },
    ];

    overlap_dedupe_snippet_sections(&mut sections);
    let snippet_count = sections
        .iter()
        .filter(|section| matches!(section, ReadPackSection::Snippet { .. }))
        .count();
    assert_eq!(snippet_count, 1, "expected contained snippet to be deduped");
}

#[test]
fn strip_reasons_keeps_focus_file_only_when_requested() {
    let mut sections = vec![
        ReadPackSection::Snippet {
            result: ReadPackSnippet {
                file: "src/main.rs".to_string(),
                start_line: 1,
                end_line: 10,
                content: "fn main() {}\n".to_string(),
                kind: Some(ReadPackSnippetKind::Code),
                reason: Some(REASON_ANCHOR_FOCUS_FILE.to_string()),
                next_cursor: None,
            },
        },
        ReadPackSection::Snippet {
            result: ReadPackSnippet {
                file: "README.md".to_string(),
                start_line: 1,
                end_line: 5,
                content: "Read me\n".to_string(),
                kind: Some(ReadPackSnippetKind::Doc),
                reason: Some(REASON_ANCHOR_DOC.to_string()),
                next_cursor: None,
            },
        },
    ];

    strip_snippet_reasons_for_output(&mut sections, true);
    let focus_reason = match &sections[0] {
        ReadPackSection::Snippet { result } => result.reason.clone(),
        _ => None,
    };
    let other_reason = match &sections[1] {
        ReadPackSection::Snippet { result } => result.reason.clone(),
        _ => None,
    };
    assert_eq!(
        focus_reason.as_deref(),
        Some(REASON_ANCHOR_FOCUS_FILE),
        "expected focus-file reason to remain when keep_focus_file=true"
    );
    assert!(
        other_reason.is_none(),
        "expected non-focus reasons to be stripped"
    );

    strip_snippet_reasons_for_output(&mut sections, false);
    let focus_reason = match &sections[0] {
        ReadPackSection::Snippet { result } => result.reason.clone(),
        _ => None,
    };
    assert!(
        focus_reason.is_none(),
        "expected focus-file reason to be stripped when keep_focus_file=false"
    );
}
