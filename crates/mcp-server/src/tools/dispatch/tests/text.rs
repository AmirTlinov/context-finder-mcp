use super::super::*;
use context_code_chunker::ChunkMetadata;

#[test]
fn word_boundary_match_hits_only_whole_identifier() {
    assert!(ContextFinderService::find_word_boundary("fn new() {}", "new").is_some());
    assert!(ContextFinderService::find_word_boundary("renew", "new").is_none());
    assert!(ContextFinderService::find_word_boundary("news", "new").is_none());
    assert!(ContextFinderService::find_word_boundary("new_", "new").is_none());
    assert!(ContextFinderService::find_word_boundary(" new ", "new").is_some());
}

#[test]
fn text_usages_compute_line_and_respect_exclusion() {
    let chunk = context_code_chunker::CodeChunk::new(
        "a.rs".to_string(),
        10,
        20,
        "fn caller() {\n  touch_daemon_best_effort();\n}\n".to_string(),
        ChunkMetadata::default()
            .symbol_name("caller")
            .chunk_type(context_code_chunker::ChunkType::Function),
    );

    let usages = ContextFinderService::find_text_usages(
        std::slice::from_ref(&chunk),
        "touch_daemon_best_effort",
        None,
        10,
    );
    assert_eq!(usages.len(), 1);
    assert_eq!(usages[0].file, "a.rs");
    assert_eq!(usages[0].line, 11);
    assert_eq!(usages[0].symbol, "caller");
    assert_eq!(usages[0].relationship, "TextMatch");

    let exclude = format!(
        "{}:{}:{}",
        chunk.file_path, chunk.start_line, chunk.end_line
    );
    let excluded = ContextFinderService::find_text_usages(
        &[chunk],
        "touch_daemon_best_effort",
        Some(&exclude),
        10,
    );
    assert!(excluded.is_empty());
}
