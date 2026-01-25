use super::super::candidates::{
    collect_github_workflow_candidates, collect_memory_file_candidates, is_disallowed_memory_file,
};
use tempfile::tempdir;

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
