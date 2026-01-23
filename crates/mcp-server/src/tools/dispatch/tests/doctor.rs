use super::super::*;
use context_code_chunker::ChunkMetadata;
use tempfile::tempdir;

#[tokio::test]
async fn doctor_manifest_parsing_reports_missing_assets() {
    let tmp = tempdir().expect("tempdir");
    let model_dir = tmp.path().join("models");
    std::fs::create_dir_all(&model_dir).unwrap();

    std::fs::write(
        model_dir.join("manifest.json"),
        r#"{"schema_version":1,"models":[{"id":"m1","assets":[{"path":"m1/model.onnx"}]}]}"#,
    )
    .unwrap();

    let (exists, models) = load_model_statuses(&model_dir).await.unwrap();
    assert!(exists);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "m1");
    assert!(!models[0].installed);
    assert_eq!(models[0].missing_assets, vec!["m1/model.onnx"]);
}

#[tokio::test]
async fn doctor_drift_helpers_detect_missing_and_extra_chunks() {
    let tmp = tempdir().expect("tempdir");
    let corpus_path = tmp.path().join("corpus.json");
    let index_path = tmp.path().join("index.json");

    let mut corpus = ChunkCorpus::new();
    corpus.set_file_chunks(
        "a.rs".to_string(),
        vec![context_code_chunker::CodeChunk::new(
            "a.rs".to_string(),
            1,
            2,
            "alpha".to_string(),
            ChunkMetadata::default(),
        )],
    );
    corpus.set_file_chunks(
        "c.rs".to_string(),
        vec![context_code_chunker::CodeChunk::new(
            "c.rs".to_string(),
            10,
            12,
            "gamma".to_string(),
            ChunkMetadata::default(),
        )],
    );
    corpus.save(&corpus_path).await.unwrap();

    // Index contains one correct chunk id (a.rs:1:2) and one extra (b.rs:1:1),
    // while missing c.rs:10:12.
    std::fs::write(
        &index_path,
        r#"{"schema_version":3,"dimension":384,"next_id":2,"id_map":{"0":"a.rs:1:2","1":"b.rs:1:1"},"vectors":{}}"#,
    )
    .unwrap();

    let corpus_ids = load_corpus_chunk_ids(&corpus_path).await.unwrap();
    let index_ids = load_index_chunk_ids(&index_path).await.unwrap();

    assert_eq!(corpus_ids.len(), 2);
    assert_eq!(index_ids.len(), 2);
    assert_eq!(corpus_ids.difference(&index_ids).count(), 1);
    assert_eq!(index_ids.difference(&corpus_ids).count(), 1);
}
