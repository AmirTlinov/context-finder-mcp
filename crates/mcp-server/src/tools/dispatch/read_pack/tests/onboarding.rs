use super::super::{
    build_context, handle_onboarding_intent, resolve_intent, ProjectFactsResult, ReadPackIntent,
    ReadPackSection, ResponseMode,
};
use super::support::base_request;
use tempfile::tempdir;

#[test]
fn auto_intent_routes_onboarding_for_onboarding_like_query() {
    let mut request = base_request();
    request.query = Some("how to run tests".to_string());
    request.intent = None;

    let intent = resolve_intent(&request).unwrap();
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
        version: 1,
        ecosystems: vec!["rust".to_string()],
        build_tools: vec!["cargo".to_string()],
        ci: Vec::new(),
        contracts: Vec::new(),
        key_dirs: Vec::new(),
        modules: Vec::new(),
        entry_points: Vec::new(),
        key_configs: Vec::new(),
    };
    handle_onboarding_intent(&ctx, &request, ResponseMode::Facts, &facts, &mut sections)
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
        version: 1,
        ecosystems: vec!["rust".to_string()],
        build_tools: vec!["cargo".to_string()],
        ci: Vec::new(),
        contracts: Vec::new(),
        key_dirs: Vec::new(),
        modules: Vec::new(),
        entry_points: Vec::new(),
        key_configs: Vec::new(),
    };
    handle_onboarding_intent(&ctx, &request, ResponseMode::Facts, &facts, &mut sections)
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
        b"# Agent rules\n\n...\n\nQuality gates:\nCONTEXT_EMBEDDING_MODE=stub cargo test --workspace\n",
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
        version: 1,
        ecosystems: vec!["rust".to_string()],
        build_tools: vec!["cargo".to_string()],
        ci: Vec::new(),
        contracts: Vec::new(),
        key_dirs: Vec::new(),
        modules: Vec::new(),
        entry_points: Vec::new(),
        key_configs: Vec::new(),
    };
    handle_onboarding_intent(&ctx, &request, ResponseMode::Facts, &facts, &mut sections)
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
