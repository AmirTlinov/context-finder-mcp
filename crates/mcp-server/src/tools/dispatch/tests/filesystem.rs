use super::super::*;
use crate::tools::schemas::list_files::ListFilesTruncation;
use tempfile::tempdir;

#[tokio::test]
async fn map_works_without_index_and_has_no_side_effects() {
    let tmp = tempdir().expect("tempdir");
    let root = tmp.path();
    let root_display = root.to_string_lossy().to_string();

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src").join("main.rs"),
        "fn main() { println!(\"hi\"); }\n",
    )
    .unwrap();

    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("docs").join("README.md"), "# Hello\n").unwrap();

    let context_dir = context_vector_store::context_dir_for_project_root(root);
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists()
    );

    let result = compute_map_result(root, &root_display, 1, 20, 0)
        .await
        .unwrap();
    assert_eq!(result.total_files, Some(2));
    assert!(result.total_chunks.unwrap_or(0) > 0);
    assert!(result.directories.iter().any(|d| d.path == "src"));
    assert!(result.directories.iter().any(|d| d.path == "docs"));
    assert!(!result.truncated);
    assert!(result.next_cursor.is_none());

    // `map` must not create indexes/corpus.
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists()
    );
}

#[tokio::test]
async fn list_files_works_without_index_and_is_bounded() {
    let tmp = tempdir().expect("tempdir");
    let root = tmp.path();
    let root_display = root.to_string_lossy().to_string();

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();

    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("docs").join("README.md"), "# Hello\n").unwrap();

    std::fs::write(root.join("README.md"), "Root\n").unwrap();

    let context_dir = context_vector_store::context_dir_for_project_root(root);
    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists()
    );

    let result = compute_list_files_result(root, &root_display, None, 50, 20_000, false, None)
        .await
        .unwrap();
    assert_eq!(result.source.as_deref(), Some("filesystem"));
    assert!(result.files.contains(&"src/main.rs".to_string()));
    assert!(result.files.contains(&"docs/README.md".to_string()));
    assert!(result.files.contains(&"README.md".to_string()));
    assert!(!result.truncated);
    assert!(result.next_cursor.is_none());

    let filtered =
        compute_list_files_result(root, &root_display, Some("docs"), 50, 20_000, false, None)
            .await
            .unwrap();
    assert_eq!(filtered.files, vec!["docs/README.md".to_string()]);
    assert!(!filtered.truncated);
    assert!(filtered.next_cursor.is_none());

    let globbed =
        compute_list_files_result(root, &root_display, Some("src/*"), 50, 20_000, false, None)
            .await
            .unwrap();
    assert_eq!(globbed.files, vec!["src/main.rs".to_string()]);
    assert!(!globbed.truncated);
    assert!(globbed.next_cursor.is_none());

    let limited = compute_list_files_result(root, &root_display, None, 1, 20_000, false, None)
        .await
        .unwrap();
    assert!(limited.truncated);
    assert_eq!(limited.truncation, Some(ListFilesTruncation::MaxItems));
    assert_eq!(limited.files.len(), 1);
    assert!(limited.next_cursor.is_some());

    let tiny = compute_list_files_result(root, &root_display, None, 50, 3, false, None)
        .await
        .unwrap();
    assert!(tiny.truncated);
    assert_eq!(tiny.truncation, Some(ListFilesTruncation::MaxChars));
    assert!(tiny.next_cursor.is_some());

    assert!(
        !context_dir.exists()
            && !root.join(".context").exists()
            && !root.join(".context-finder").exists()
    );
}
