use super::types::TextSearchOutcome;
use crate::tools::schemas::text_search::TextSearchMatch;

#[test]
fn text_search_dedupes_matches() {
    let mut outcome = TextSearchOutcome::new();
    let first = TextSearchMatch {
        file: "src/main.rs".to_string(),
        line: 1,
        column: 1,
        text: "fn main() {}".to_string(),
    };
    assert!(outcome.push_match(first));

    let dup = TextSearchMatch {
        file: "src/main.rs".to_string(),
        line: 1,
        column: 1,
        text: "fn main() {}".to_string(),
    };
    assert!(!outcome.push_match(dup));
    assert_eq!(outcome.matches.len(), 1);
    assert_eq!(outcome.matched_files.len(), 1);
}
