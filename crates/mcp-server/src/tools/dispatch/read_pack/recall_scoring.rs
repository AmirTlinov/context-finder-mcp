use super::cursors::snippet_kind_for_path;
use super::{ReadPackSnippet, ReadPackSnippetKind};

pub(super) fn score_recall_snippet(question_tokens: &[String], snippet: &ReadPackSnippet) -> i32 {
    if question_tokens.is_empty() {
        return 0;
    }
    let file = snippet.file.to_ascii_lowercase();
    let content = snippet.content.to_lowercase();
    let mut score = 0i32;

    for token in question_tokens {
        if file.contains(token) {
            score += 3;
        }
        if content.contains(token) {
            score += 5;
        }
    }

    // Small heuristic boost: snippets with runnable commands are usually better for ops recall.
    if content.contains("cargo ") || content.contains("npm ") || content.contains("yarn ") {
        score += 1;
    }
    if content.contains("docker ") || content.contains("kubectl ") || content.contains("make ") {
        score += 1;
    }

    score
}

pub(super) fn recall_has_code_snippet(snippets: &[ReadPackSnippet]) -> bool {
    snippets
        .iter()
        .any(|snippet| snippet_kind_for_path(&snippet.file) == ReadPackSnippetKind::Code)
}
