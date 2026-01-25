use super::super::context::build_context;
use super::super::ContextFinderService;
use super::super::{handle_memory_intent, ProjectFactsResult, ReadPackSection, ResponseMode};
use super::support::base_request;
use tempfile::tempdir;

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
    handle_memory_intent(
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

    handle_memory_intent(
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
