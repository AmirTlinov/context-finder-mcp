use super::super::*;

#[test]
fn batch_prepare_item_input_injects_max_chars_for_ls() {
    let input = serde_json::json!({});
    let prepared = prepare_item_input(input, Some("/root"), BatchToolName::Ls, 5_000);

    let obj = prepared.as_object().expect("prepared input must be object");
    assert_eq!(obj.get("path").and_then(|v| v.as_str()), Some("/root"));
    assert!(
        obj.get("max_chars").is_some(),
        "expected max_chars to be injected for ls"
    );
}

#[test]
fn batch_prepare_item_input_injects_max_chars_for_rg() {
    let input = serde_json::json!({});
    let prepared = prepare_item_input(input, Some("/root"), BatchToolName::Rg, 5_000);

    let obj = prepared.as_object().expect("prepared input must be object");
    assert_eq!(obj.get("path").and_then(|v| v.as_str()), Some("/root"));
    assert!(
        obj.get("max_chars").is_some(),
        "expected max_chars to be injected for rg"
    );
}
