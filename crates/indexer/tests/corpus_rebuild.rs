use context_indexer::{ModelIndexSpec, MultiModelProjectIndexer, ProjectIndexer};
use context_vector_store::{context_dir_for_project_root, EmbeddingTemplates, VectorIndex};
use tempfile::TempDir;

fn index_path(root: &std::path::Path, model_id: &str) -> std::path::PathBuf {
    context_dir_for_project_root(root)
        .join("indexes")
        .join(model_id)
        .join("index.json")
}

fn corpus_path(root: &std::path::Path) -> std::path::PathBuf {
    context_dir_for_project_root(root).join("corpus.json")
}

fn has_chunk_for_file(index: &VectorIndex, file_prefix: &str) -> bool {
    index
        .chunk_ids()
        .iter()
        .any(|chunk_id| chunk_id.starts_with(file_prefix))
}

#[tokio::test]
async fn project_indexer_rebuilds_store_when_corpus_is_missing() {
    std::env::set_var("CONTEXT_EMBEDDING_MODE", "stub");

    let temp = TempDir::new().expect("tempdir");
    let src_dir = temp.path().join("src");
    tokio::fs::create_dir_all(&src_dir)
        .await
        .expect("create src");
    tokio::fs::write(
        src_dir.join("lib.rs"),
        r#"
pub fn hello() {
    println!("hello");
}

pub fn world() {
    println!("world");
}
"#,
    )
    .await
    .expect("write file");

    let indexer = ProjectIndexer::new(temp.path()).await.expect("indexer");
    indexer.index_full().await.expect("initial index");

    let path = index_path(temp.path(), "bge-small");
    let index = VectorIndex::load(&path).await.expect("load index");
    assert!(
        has_chunk_for_file(&index, "src/lib.rs:"),
        "missing src/lib.rs chunk"
    );

    tokio::fs::remove_file(corpus_path(temp.path()))
        .await
        .expect("delete corpus");

    indexer.index().await.expect("incremental rebuild");

    let index = VectorIndex::load(&path).await.expect("load rebuilt index");
    assert!(
        has_chunk_for_file(&index, "src/lib.rs:"),
        "missing src/lib.rs chunk after corpus rebuild"
    );
}

#[tokio::test]
async fn multimodel_indexer_rebuilds_all_models_when_corpus_is_missing() {
    std::env::set_var("CONTEXT_EMBEDDING_MODE", "stub");

    let temp = TempDir::new().expect("tempdir");
    let src_dir = temp.path().join("src");
    tokio::fs::create_dir_all(&src_dir)
        .await
        .expect("create src");
    tokio::fs::write(
        src_dir.join("lib.rs"),
        r#"
pub fn hello() {
    println!("hello");
}

pub fn world() {
    println!("world");
}
"#,
    )
    .await
    .expect("write file");

    let indexer = MultiModelProjectIndexer::new(temp.path())
        .await
        .expect("multimodel indexer");

    let templates = EmbeddingTemplates::default();
    let models = vec![
        ModelIndexSpec::new("bge-small", templates.clone()),
        ModelIndexSpec::new("multilingual-e5-small", templates),
    ];

    indexer
        .index_models(&models, true)
        .await
        .expect("initial index");

    tokio::fs::remove_file(corpus_path(temp.path()))
        .await
        .expect("delete corpus");

    indexer
        .index_models(&models, false)
        .await
        .expect("incremental rebuild");

    for model_id in ["bge-small", "multilingual-e5-small"] {
        let path = index_path(temp.path(), model_id);
        let index = VectorIndex::load(&path)
            .await
            .unwrap_or_else(|e| panic!("load index for {model_id}: {e}"));
        assert!(
            has_chunk_for_file(&index, "src/lib.rs:"),
            "missing src/lib.rs chunk for {model_id} after corpus rebuild"
        );
    }
}
