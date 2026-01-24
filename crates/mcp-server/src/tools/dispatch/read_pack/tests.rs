use super::super::router::cursor_alias::expand_cursor_alias;
use super::super::{decode_cursor, ContextFinderService};
use super::candidates::collect_github_workflow_candidates;
use super::cursors::ReadPackRecallCursorV1;
use super::intent_recall::{
    best_keyword_pattern, parse_recall_question_directives, recall_question_policy,
    snippets_from_grep_filtered, GrepSnippetParams, RecallQuestionMode,
};
use super::project_facts::PROJECT_FACTS_VERSION;
use super::{
    build_context, collect_memory_file_candidates, finalize_and_trim, handle_recall_intent,
    is_disallowed_memory_file, render_read_pack_context_doc, repair_recall_cursor_after_trim,
    ProjectFactsResult, ReadPackBudget, ReadPackIntent, ReadPackRecallResult, ReadPackRequest,
    ReadPackResult, ReadPackSection, ReadPackSnippet, ReadPackSnippetKind, ReadPackTruncation,
    ResponseMode,
};
use context_protocol::ToolNextAction;
use std::path::PathBuf;
use tempfile::tempdir;

fn base_request() -> ReadPackRequest {
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

#[test]
fn render_read_pack_renders_context_pack_and_next_actions_in_text() {
    let pack = context_search::ContextPackOutput {
        version: 1,
        query: "find alpha entrypoint".to_string(),
        model_id: "stub".to_string(),
        profile: "quality".to_string(),
        items: vec![context_search::ContextPackItem {
            id: "i0".to_string(),
            role: "primary".to_string(),
            file: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 3,
            symbol: Some("alpha".to_string()),
            chunk_type: Some("code".to_string()),
            score: 0.9,
            imports: Vec::new(),
            content: "pub fn alpha() -> i32 { 1 }\n".to_string(),
            relationship: None,
            distance: None,
        }],
        budget: context_search::ContextPackBudget {
            max_chars: 2000,
            used_chars: 200,
            truncated: false,
            dropped_items: 0,
            truncation: None,
        },
        next_actions: vec![ToolNextAction {
            tool: "cat".to_string(),
            args: serde_json::json!({ "file": "src/lib.rs", "start_line": 1, "max_lines": 40 }),
            reason: "Open the referenced file for more context.".to_string(),
        }],
        meta: context_indexer::ToolMeta::default(),
    };

    let result = ReadPackResult {
        version: 1,
        intent: ReadPackIntent::Query,
        root: ".".to_string(),
        sections: vec![ReadPackSection::ContextPack {
            result: serde_json::to_value(&pack).expect("pack should serialize"),
        }],
        next_actions: vec![ToolNextAction {
            tool: "read_pack".to_string(),
            args: serde_json::json!({ "intent": "query", "query": "alpha", "max_chars": 4000 }),
            reason: "Retry with a larger budget.".to_string(),
        }],
        next_cursor: None,
        budget: ReadPackBudget {
            max_chars: 2000,
            used_chars: 200,
            truncated: false,
            truncation: None,
        },
        meta: None,
    };

    let text = render_read_pack_context_doc(&result, ResponseMode::Full);
    assert!(
        text.contains("context_pack:"),
        "expected context_pack summary, got:\n{text}"
    );
    assert!(
        text.contains("R: src/lib.rs:1"),
        "expected item file ref, got:\n{text}"
    );
    assert!(
        text.contains("next_actions:"),
        "expected next_actions section, got:\n{text}"
    );
    assert!(
        !text.contains("structured_content"),
        "must not mention structured_content in text output:\n{text}"
    );
}

#[test]
fn build_context_reserves_headroom() {
    let mut request = base_request();
    request.max_chars = Some(20_000);

    let ctx = build_context(&request, PathBuf::from("."), ".".to_string())
        .unwrap_or_else(|_| panic!("build_context should succeed"));
    assert_eq!(ctx.inner_max_chars, 19_200);
}

#[test]
fn build_context_never_exceeds_max_chars() {
    let mut request = base_request();
    request.max_chars = Some(500);

    let ctx = build_context(&request, PathBuf::from("."), ".".to_string())
        .unwrap_or_else(|_| panic!("build_context should succeed"));
    assert_eq!(ctx.max_chars, 500);
    assert_eq!(ctx.inner_max_chars, 436);
}

#[test]
fn memory_candidates_block_secrets_allow_templates() {
    assert!(is_disallowed_memory_file(".env"));
    assert!(is_disallowed_memory_file(".env.local"));
    assert!(is_disallowed_memory_file("prod.env"));
    assert!(is_disallowed_memory_file("id_rsa"));
    assert!(is_disallowed_memory_file("secrets/id_ed25519"));
    assert!(is_disallowed_memory_file("cert.pem"));
    assert!(is_disallowed_memory_file("keys/token.pfx"));

    assert!(!is_disallowed_memory_file(".env.example"));
    assert!(!is_disallowed_memory_file(".env.sample"));
    assert!(!is_disallowed_memory_file(".env.template"));
    assert!(!is_disallowed_memory_file(".env.dist"));
}

#[test]
fn github_workflow_candidates_are_sorted_and_bounded() {
    let temp = tempdir().unwrap();
    let workflows_dir = temp.path().join(".github").join("workflows");
    std::fs::create_dir_all(&workflows_dir).unwrap();

    std::fs::write(workflows_dir.join("b.yml"), b"name: b\n").unwrap();
    std::fs::write(workflows_dir.join("a.yaml"), b"name: a\n").unwrap();
    std::fs::write(workflows_dir.join("c.txt"), b"ignore\n").unwrap();

    let mut seen = std::collections::HashSet::new();
    let candidates = collect_github_workflow_candidates(temp.path(), &mut seen);

    assert_eq!(
        candidates,
        vec![".github/workflows/a.yaml", ".github/workflows/b.yml"]
    );
}

#[test]
fn memory_candidates_fallback_discovers_doc_like_files() {
    let temp = tempdir().unwrap();
    std::fs::write(temp.path().join("HACKING.md"), b"how to hack\n").unwrap();

    let candidates = collect_memory_file_candidates(temp.path());
    assert!(
        candidates.iter().any(|c| c == "HACKING.md"),
        "expected fallback doc discovery to include HACKING.md"
    );
}

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
                reason: Some(super::REASON_NEEDLE_FILE_SLICE.to_string()),
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
                reason: Some(super::REASON_NEEDLE_FILE_SLICE.to_string()),
                next_cursor: None,
            },
        },
    ];

    super::overlap_dedupe_snippet_sections(&mut sections);
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
                reason: Some(super::REASON_ANCHOR_FOCUS_FILE.to_string()),
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
                reason: Some(super::REASON_ANCHOR_DOC.to_string()),
                next_cursor: None,
            },
        },
    ];

    super::strip_snippet_reasons_for_output(&mut sections, true);
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
        Some(super::REASON_ANCHOR_FOCUS_FILE),
        "expected focus-file reason to remain when keep_focus_file=true"
    );
    assert!(
        other_reason.is_none(),
        "expected non-focus reasons to be stripped"
    );

    super::strip_snippet_reasons_for_output(&mut sections, false);
    let focus_reason = match &sections[0] {
        ReadPackSection::Snippet { result } => result.reason.clone(),
        _ => None,
    };
    assert!(
        focus_reason.is_none(),
        "expected focus-file reason to be stripped when keep_focus_file=false"
    );
}

#[tokio::test]
async fn memory_pack_prefers_unseen_docs_across_calls() {
    let service = ContextFinderService::new();

    let temp = tempdir().unwrap();
    let root = temp.path();

    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
    std::fs::create_dir_all(root.join(".vscode")).unwrap();

    std::fs::write(root.join("AGENTS.md"), b"agents\n").unwrap();
    std::fs::write(root.join("README.md"), b"readme\n").unwrap();
    std::fs::write(root.join("docs/README.md"), b"docs readme\n").unwrap();
    std::fs::write(root.join("docs/QUICK_START.md"), b"quick start\n").unwrap();
    std::fs::write(root.join("PHILOSOPHY.md"), b"philosophy\n").unwrap();
    std::fs::write(root.join("DEVELOPMENT.md"), b"dev\n").unwrap();
    std::fs::write(root.join("Cargo.toml"), b"[package]\nname = \"x\"\n").unwrap();
    std::fs::write(
        root.join(".github/workflows/ci.yml"),
        b"name: CI\non: [push]\n",
    )
    .unwrap();
    std::fs::write(root.join(".vscode/settings.json"), b"{\"x\":1}\n").unwrap();

    let root_display = root.to_string_lossy().to_string();
    let mut request = base_request();
    request.path = Some(root_display.clone());
    request.max_chars = Some(8_000);
    request.response_mode = Some(ResponseMode::Facts);

    let ctx = build_context(&request, root.to_path_buf(), root_display.clone()).unwrap();

    let mut sections1 = vec![ReadPackSection::ProjectFacts {
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
    }];
    let mut next_actions = Vec::new();
    let mut next_cursor = None;
    super::handle_memory_intent(
        &service,
        &ctx,
        &request,
        ResponseMode::Facts,
        &mut sections1,
        &mut next_actions,
        &mut next_cursor,
    )
    .await
    .unwrap();

    let files1: Vec<String> = sections1
        .iter()
        .filter_map(|section| match section {
            ReadPackSection::Snippet { result } => Some(result.file.clone()),
            ReadPackSection::FileSlice { result } => Some(result.file.clone()),
            _ => None,
        })
        .collect();
    assert!(
        files1.iter().any(|f| f == "AGENTS.md"),
        "expected AGENTS.md in first memory pack"
    );
    assert!(
        files1.iter().any(|f| f == "README.md"),
        "expected README.md in first memory pack"
    );

    {
        let mut session = service.session.lock().await;
        for file in &files1 {
            session.note_seen_snippet_file(file);
        }
    }

    let mut sections2 = vec![ReadPackSection::ProjectFacts {
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
    }];
    let mut next_actions = Vec::new();

    let mut next_cursor = None;
    super::handle_memory_intent(
        &service,
        &ctx,
        &request,
        ResponseMode::Facts,
        &mut sections2,
        &mut next_actions,
        &mut next_cursor,
    )
    .await
    .unwrap();

    let files2: Vec<String> = sections2
        .iter()
        .filter_map(|section| match section {
            ReadPackSection::Snippet { result } => Some(result.file.clone()),
            ReadPackSection::FileSlice { result } => Some(result.file.clone()),
            _ => None,
        })
        .collect();
    assert!(
        files2.iter().any(|f| f == "AGENTS.md"),
        "expected AGENTS.md in second memory pack (anchor)"
    );
    assert!(
        files2.iter().any(|f| f == "README.md"),
        "expected README.md in second memory pack (anchor)"
    );

    let non_anchor1: std::collections::HashSet<String> = files1
        .into_iter()
        .filter(|f| f != "AGENTS.md" && f != "README.md")
        .collect();
    let non_anchor2: std::collections::HashSet<String> = files2
        .into_iter()
        .filter(|f| f != "AGENTS.md" && f != "README.md")
        .collect();
    assert!(
        non_anchor2.difference(&non_anchor1).next().is_some(),
        "expected second memory pack to include at least one new non-anchor file"
    );
}

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

#[test]
fn auto_intent_routes_onboarding_for_onboarding_like_query() {
    let mut request = base_request();
    request.query = Some("how to run tests".to_string());
    request.intent = None;

    let intent = super::resolve_intent(&request).unwrap();
    assert_eq!(intent, ReadPackIntent::Onboarding);
}

#[tokio::test]
async fn onboarding_intent_in_facts_mode_emits_snippets() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join("README.md"), b"## Quick start\nrun tests\n").unwrap();
    std::fs::write(root.join("AGENTS.md"), b"agents\n").unwrap();

    let mut request = base_request();
    request.path = Some(root.to_string_lossy().to_string());
    request.max_chars = Some(4_000);

    let ctx = build_context(&request, root.to_path_buf(), request.path.clone().unwrap())
        .unwrap_or_else(|_| panic!("build_context should succeed"));

    let mut sections = Vec::new();
    let facts = ProjectFactsResult {
        version: PROJECT_FACTS_VERSION,
        ecosystems: vec!["rust".to_string()],
        build_tools: vec!["cargo".to_string()],
        ci: Vec::new(),
        contracts: Vec::new(),
        key_dirs: Vec::new(),
        modules: Vec::new(),
        entry_points: Vec::new(),
        key_configs: Vec::new(),
    };
    super::handle_onboarding_intent(&ctx, &request, ResponseMode::Facts, &facts, &mut sections)
        .await
        .unwrap();

    assert!(
        sections
            .iter()
            .any(|s| matches!(s, ReadPackSection::Snippet { .. })),
        "expected onboarding to emit snippet sections in facts mode"
    );
    assert!(
        !sections
            .iter()
            .any(|s| matches!(s, ReadPackSection::RepoOnboardingPack { .. })),
        "expected onboarding not to emit full repo_onboarding_pack section in facts mode"
    );
}

#[tokio::test]
async fn onboarding_facts_tight_budget_still_emits_anchor_doc_snippet() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join("AGENTS.md"), b"# AGENTS\n\nline\nline\nline\n").unwrap();

    let mut request = base_request();
    request.path = Some(root.to_string_lossy().to_string());
    request.max_chars = Some(1_200);

    let ctx = build_context(&request, root.to_path_buf(), request.path.clone().unwrap())
        .unwrap_or_else(|_| panic!("build_context should succeed"));

    let mut sections = Vec::new();
    let facts = ProjectFactsResult {
        version: PROJECT_FACTS_VERSION,
        ecosystems: vec!["rust".to_string()],
        build_tools: vec!["cargo".to_string()],
        ci: Vec::new(),
        contracts: Vec::new(),
        key_dirs: Vec::new(),
        modules: Vec::new(),
        entry_points: Vec::new(),
        key_configs: Vec::new(),
    };
    super::handle_onboarding_intent(&ctx, &request, ResponseMode::Facts, &facts, &mut sections)
        .await
        .unwrap();

    assert!(
        sections
            .iter()
            .any(|s| matches!(s, ReadPackSection::Snippet { .. })),
        "expected onboarding facts to emit at least one snippet under a tight budget"
    );
}

#[tokio::test]
async fn onboarding_tests_question_emits_command_snippet_via_grep() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    std::fs::write(
        root.join("AGENTS.md"),
        b"# Agent rules\n\n...\n\nQuality gates:\nCONTEXT_FINDER_EMBEDDING_MODE=stub cargo test --workspace\n",
    )
    .unwrap();

    let mut request = base_request();
    request.path = Some(root.to_string_lossy().to_string());
    request.ask = Some("how to run tests".to_string());
    request.max_chars = Some(1_800);

    let ctx = build_context(&request, root.to_path_buf(), request.path.clone().unwrap())
        .unwrap_or_else(|_| panic!("build_context should succeed"));

    let mut sections = Vec::new();
    let facts = ProjectFactsResult {
        version: PROJECT_FACTS_VERSION,
        ecosystems: vec!["rust".to_string()],
        build_tools: vec!["cargo".to_string()],
        ci: Vec::new(),
        contracts: Vec::new(),
        key_dirs: Vec::new(),
        modules: Vec::new(),
        entry_points: Vec::new(),
        key_configs: Vec::new(),
    };
    super::handle_onboarding_intent(&ctx, &request, ResponseMode::Facts, &facts, &mut sections)
        .await
        .unwrap();

    let found = sections.iter().any(|section| match section {
        ReadPackSection::Snippet { result } => result.content.contains("cargo test"),
        _ => false,
    });
    assert!(
        found,
        "expected onboarding to surface a test command via grep snippet"
    );
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

    let temp = tempdir().unwrap();
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
