use super::super::*;
use context_code_chunker::ChunkMetadata;
use context_search::{EnrichedResult, RelatedContext};
use context_vector_store::SearchResult;

#[test]
fn context_pack_prefers_more_primary_items_under_tight_budgets() {
    let make_chunk = |file: &str, start: usize, end: usize, symbol: &str, content_len: usize| {
        let content = "x".repeat(content_len);
        context_code_chunker::CodeChunk::new(
            file.to_string(),
            start,
            end,
            content,
            ChunkMetadata::default()
                .symbol_name(symbol)
                .chunk_type(context_code_chunker::ChunkType::Function),
        )
    };

    let primary = |file: &str, id: &str, symbol: &str| SearchResult {
        chunk: make_chunk(file, 1, 10, symbol, 40),
        score: 1.0,
        id: id.to_string(),
    };

    let related = |file: &str, symbol: &str| RelatedContext {
        chunk: make_chunk(file, 1, 200, symbol, 1_000),
        relationship_path: vec!["Calls".to_string()],
        distance: 1,
        relevance_score: 0.5,
    };

    let enriched = vec![
        EnrichedResult {
            primary: primary("src/a.rs", "src/a.rs:1:10", "a"),
            related: vec![related("src/a_related.rs", "a_related")],
            total_lines: 10,
            strategy: context_graph::AssemblyStrategy::Direct,
        },
        EnrichedResult {
            primary: primary("src/b.rs", "src/b.rs:1:10", "b"),
            related: vec![related("src/b_related.rs", "b_related")],
            total_lines: 10,
            strategy: context_graph::AssemblyStrategy::Direct,
        },
        EnrichedResult {
            primary: primary("src/c.rs", "src/c.rs:1:10", "c"),
            related: vec![related("src/c_related.rs", "c_related")],
            total_lines: 10,
            strategy: context_graph::AssemblyStrategy::Direct,
        },
    ];

    let profile = SearchProfile::general();
    let max_chars = 900;
    let (items, budget) = pack_enriched_results(
        &profile,
        enriched,
        max_chars,
        3,
        &[],
        &[],
        None,
        RelatedMode::Explore,
        &[],
    );

    let primary_count = items.iter().filter(|i| i.role == "primary").count();
    assert_eq!(primary_count, 3, "expected all primaries to fit");
    assert!(
        items.iter().take(3).all(|i| i.role == "primary"),
        "expected primaries to be emitted before related items"
    );
    assert!(
        budget.used_chars <= max_chars,
        "expected budget.used_chars <= max_chars"
    );
    assert!(
        budget.truncated,
        "expected related items to trigger truncation under tight max_chars"
    );
}

#[test]
fn context_pack_never_returns_zero_items_when_first_chunk_is_huge() {
    let chunk = context_code_chunker::CodeChunk::new(
        "src/big.rs".to_string(),
        1,
        999,
        "x".repeat(10_000),
        ChunkMetadata::default()
            .symbol_name("huge")
            .chunk_type(context_code_chunker::ChunkType::Function),
    );

    let enriched = vec![EnrichedResult {
        primary: SearchResult {
            chunk,
            score: 1.0,
            id: "src/big.rs:1:999".to_string(),
        },
        related: vec![],
        total_lines: 999,
        strategy: context_graph::AssemblyStrategy::Direct,
    }];

    let profile = SearchProfile::general();
    let max_chars = 1_000;
    let (items, budget) = pack_enriched_results(
        &profile,
        enriched,
        max_chars,
        0,
        &[],
        &[],
        None,
        RelatedMode::Explore,
        &[],
    );

    assert_eq!(items.len(), 1, "expected an anchor item");
    assert_eq!(items[0].role, "primary");
    assert!(
        !items[0].content.is_empty(),
        "anchor content should be non-empty"
    );
    assert!(
        budget.truncated,
        "expected truncation when first chunk exceeds max_chars"
    );
}

fn mk_chunk(file_path: &str, start_line: usize, content: &str) -> context_code_chunker::CodeChunk {
    context_code_chunker::CodeChunk::new(
        file_path.to_string(),
        start_line,
        start_line + content.lines().count().saturating_sub(1),
        content.to_string(),
        ChunkMetadata::default(),
    )
}

#[test]
fn prepare_excludes_docs_when_disabled() {
    let primary_code = SearchResult {
        id: "src/main.rs:1:1".to_string(),
        chunk: mk_chunk("src/main.rs", 1, "fn main() {}"),
        score: 0.9,
    };
    let primary_docs = SearchResult {
        id: "docs/readme.md:1:1".to_string(),
        chunk: mk_chunk("docs/readme.md", 1, "# docs"),
        score: 1.0,
    };

    let related_docs = RelatedContext {
        chunk: mk_chunk("docs/guide.md", 1, "# guide"),
        relationship_path: vec!["Calls".to_string()],
        distance: 1,
        relevance_score: 0.5,
    };
    let related_code = RelatedContext {
        chunk: mk_chunk("src/lib.rs", 10, "pub fn f() {}"),
        relationship_path: vec!["Calls".to_string()],
        distance: 1,
        relevance_score: 0.6,
    };

    let enriched = vec![
        EnrichedResult {
            primary: primary_docs,
            related: Vec::new(),
            total_lines: 1,
            strategy: context_graph::AssemblyStrategy::Extended,
        },
        EnrichedResult {
            primary: primary_code,
            related: vec![related_docs, related_code],
            total_lines: 1,
            strategy: context_graph::AssemblyStrategy::Extended,
        },
    ];

    let prepared = prepare_context_pack_enriched(enriched, 10, false, false);
    let files: Vec<&str> = prepared
        .iter()
        .map(|er| er.primary.chunk.file_path.as_str())
        .collect();
    assert_eq!(files, vec!["src/main.rs"]);

    let related_files: Vec<&str> = prepared[0]
        .related
        .iter()
        .map(|rc| rc.chunk.file_path.as_str())
        .collect();
    assert_eq!(related_files, vec!["src/lib.rs"]);
}

#[test]
fn prepare_prefers_code_over_docs_when_enabled() {
    let primary_code = SearchResult {
        id: "src/main.rs:1:1".to_string(),
        chunk: mk_chunk("src/main.rs", 1, "fn main() {}"),
        score: 0.9,
    };
    let primary_docs = SearchResult {
        id: "docs/readme.md:1:1".to_string(),
        chunk: mk_chunk("docs/readme.md", 1, "# docs"),
        score: 1.0,
    };

    let enriched = vec![
        EnrichedResult {
            primary: primary_docs,
            related: Vec::new(),
            total_lines: 1,
            strategy: context_graph::AssemblyStrategy::Extended,
        },
        EnrichedResult {
            primary: primary_code,
            related: Vec::new(),
            total_lines: 1,
            strategy: context_graph::AssemblyStrategy::Extended,
        },
    ];

    let prepared = prepare_context_pack_enriched(enriched, 10, true, true);
    let files: Vec<&str> = prepared
        .iter()
        .map(|er| er.primary.chunk.file_path.as_str())
        .collect();
    assert_eq!(files, vec!["src/main.rs", "docs/readme.md"]);
}

#[test]
fn focus_related_prefers_query_hits_over_raw_relevance() {
    let related_miss = RelatedContext {
        chunk: mk_chunk("src/miss.rs", 1, "fn unrelated() {}"),
        relationship_path: vec!["Calls".to_string()],
        distance: 1,
        relevance_score: 100.0,
    };
    let related_hit = RelatedContext {
        chunk: mk_chunk("src/hit.rs", 1, "fn target() {}"),
        relationship_path: vec!["Calls".to_string()],
        distance: 1,
        relevance_score: 0.1,
    };

    let query_tokens = vec!["target".to_string()];
    let prepared = prepare_related_contexts(
        vec![related_miss, related_hit],
        RelatedMode::Focus,
        &query_tokens,
    );
    assert_eq!(prepared[0].chunk.file_path, "src/hit.rs");
}
