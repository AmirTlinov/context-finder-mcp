use context_code_chunker::{ChunkType, Chunker, ChunkerConfig};

fn chunk(code: &str) -> Vec<context_code_chunker::CodeChunk> {
    let config = ChunkerConfig {
        min_chunk_tokens: 0, // Don't filter out small test chunks
        ..ChunkerConfig::default()
    };
    let chunker = Chunker::new(config);
    chunker
        .chunk_str(code, Some("nested.rs"))
        .expect("chunking failed")
}

#[test]
fn extracts_methods_inside_module_impl() {
    let code = r"
mod api {
    pub struct Car;

    impl Car {
        pub fn drive(&self) {}
        fn stop(&self) {}
    }
}
";

    let chunks = chunk(code);
    let methods: Vec<_> = chunks
        .iter()
        .filter(|c| c.metadata.chunk_type == Some(ChunkType::Method))
        .filter_map(|c| c.metadata.symbol_name.as_deref())
        .collect();

    assert!(
        methods.contains(&"drive") && methods.contains(&"stop"),
        "expected method chunks inside module impl, got: {methods:?}"
    );
}

#[test]
fn real_embeddings_rs_has_method_chunks() {
    let code = include_str!("../../vector-store/src/embeddings.rs");

    let chunks = chunk(code);
    let has_cosine = chunks.iter().any(|c| {
        c.metadata.chunk_type == Some(ChunkType::Method)
            && c.metadata.symbol_name.as_deref() == Some("cosine_similarity")
    });
    assert!(
        has_cosine,
        "cosine_similarity should be extracted as a method chunk from impl"
    );
}
