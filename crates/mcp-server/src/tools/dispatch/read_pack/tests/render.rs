use super::super::{
    render_read_pack_context_doc, ReadPackBudget, ReadPackIntent, ReadPackResult, ReadPackSection,
    ResponseMode,
};
use context_protocol::ToolNextAction;

#[test]
fn render_read_pack_renders_context_pack_and_next_actions_in_text() {
    let pack = context_search::ContextPackOutput {
        version: 1,
        query: "find alpha entrypoint".to_string(),
        model_id: "stub".to_string(),
        profile: "quality".to_string(),
        items: vec![context_search::ContextPackItem {
            id: "i0".to_string(),
            role: "primary".to_string(),
            file: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 3,
            symbol: Some("alpha".to_string()),
            chunk_type: Some("code".to_string()),
            score: 0.9,
            imports: Vec::new(),
            content: "pub fn alpha() -> i32 { 1 }\n".to_string(),
            relationship: None,
            distance: None,
        }],
        budget: context_search::ContextPackBudget {
            max_chars: 2000,
            used_chars: 200,
            truncated: false,
            dropped_items: 0,
            truncation: None,
        },
        next_actions: vec![ToolNextAction {
            tool: "cat".to_string(),
            args: serde_json::json!({ "file": "src/lib.rs", "start_line": 1, "max_lines": 40 }),
            reason: "Open the referenced file for more context.".to_string(),
        }],
        meta: context_indexer::ToolMeta::default(),
    };

    let result = ReadPackResult {
        version: 1,
        intent: ReadPackIntent::Query,
        root: ".".to_string(),
        sections: vec![ReadPackSection::ContextPack {
            result: serde_json::to_value(&pack).expect("pack should serialize"),
        }],
        next_actions: vec![ToolNextAction {
            tool: "read_pack".to_string(),
            args: serde_json::json!({ "intent": "query", "query": "alpha", "max_chars": 4000 }),
            reason: "Retry with a larger budget.".to_string(),
        }],
        next_cursor: None,
        budget: ReadPackBudget {
            max_chars: 2000,
            used_chars: 200,
            truncated: false,
            truncation: None,
        },
        meta: None,
    };

    let text = render_read_pack_context_doc(&result, ResponseMode::Full);
    assert!(
        text.contains("context_pack:"),
        "expected context_pack summary, got:\n{text}"
    );
    assert!(
        text.contains("R: src/lib.rs:1"),
        "expected item file ref, got:\n{text}"
    );
    assert!(
        text.contains("next_actions:"),
        "expected next_actions section, got:\n{text}"
    );
    assert!(
        !text.contains("structured_content"),
        "must not mention structured_content in text output:\n{text}"
    );
}
