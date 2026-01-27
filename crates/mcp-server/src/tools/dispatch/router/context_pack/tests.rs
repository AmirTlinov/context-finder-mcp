use super::budget::enforce_context_pack_budget;
use super::inputs::parse_inputs;
use crate::tools::dispatch::ContextPackRequest;
use context_indexer::ToolMeta;
use context_search::{ContextPackBudget, ContextPackItem, ContextPackOutput};

#[test]
fn candidate_limit_expands_for_docs_first() {
    let request = ContextPackRequest {
        query: "README".to_string(),
        path: None,
        limit: Some(5),
        max_chars: None,
        include_paths: None,
        exclude_paths: None,
        file_pattern: None,
        max_related_per_primary: None,
        prefer_code: Some(false),
        include_docs: Some(true),
        related_mode: None,
        strategy: None,
        language: None,
        response_mode: None,
        trace: None,
        auto_index: None,
        auto_index_budget_ms: None,
    };
    let inputs = parse_inputs(&request).unwrap_or_else(|_| panic!("parse_inputs should succeed"));
    assert_eq!(inputs.candidate_limit, 105);
}

#[test]
fn candidate_limit_expands_for_code_first() {
    let request = ContextPackRequest {
        query: "EmbeddingCache".to_string(),
        path: None,
        limit: Some(10),
        max_chars: None,
        include_paths: None,
        exclude_paths: None,
        file_pattern: None,
        max_related_per_primary: None,
        prefer_code: Some(true),
        include_docs: Some(true),
        related_mode: None,
        strategy: None,
        language: None,
        response_mode: None,
        trace: None,
        auto_index: None,
        auto_index_budget_ms: None,
    };
    let inputs = parse_inputs(&request).unwrap_or_else(|_| panic!("parse_inputs should succeed"));
    assert_eq!(inputs.candidate_limit, 60);
}

#[test]
fn enforce_budget_shrinks_last_item_instead_of_dropping_to_zero() {
    let mut output = ContextPackOutput {
        version: 1,
        query: "q".to_string(),
        model_id: "m".to_string(),
        profile: "p".to_string(),
        items: vec![ContextPackItem {
            id: "id".to_string(),
            role: "primary".to_string(),
            file: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 1,
            symbol: Some("alpha".to_string()),
            chunk_type: Some("Function".to_string()),
            score: 1.0,
            imports: vec!["std::fmt".to_string()],
            content: "x".repeat(10_000),
            relationship: None,
            distance: None,
        }],
        budget: ContextPackBudget {
            max_chars: 1_000,
            used_chars: 0,
            truncated: false,
            dropped_items: 0,
            truncation: None,
        },
        next_actions: Vec::new(),
        meta: ToolMeta::default(),
    };

    let result = enforce_context_pack_budget(&mut output);
    assert!(result.is_ok(), "expected budget enforcement to succeed");
    assert_eq!(output.items.len(), 1, "expected an anchor item to remain");
    assert!(
        output.budget.truncated,
        "expected truncation under tight max_chars"
    );
    assert!(
        output.items[0].content.len() < 10_000,
        "expected item content to be shrunk"
    );
}
