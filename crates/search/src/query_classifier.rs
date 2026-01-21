#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryType {
    /// Looks like a symbol/function/class name
    Identifier,
    /// Contains path separators or file-like patterns
    Path,
    /// Multi-word or conceptual description
    Conceptual,
}

#[derive(Debug, Clone, Copy)]
pub struct QueryWeights {
    pub semantic: f32,
    pub fuzzy: f32,
    pub candidate_multiplier: usize,
}

impl QueryWeights {
    #[must_use]
    pub const fn new(semantic: f32, fuzzy: f32, candidate_multiplier: usize) -> Self {
        Self {
            semantic,
            fuzzy,
            candidate_multiplier,
        }
    }
}

pub struct QueryClassifier;

fn has_file_extension(token: &str) -> bool {
    let token = token.trim();
    let Some((_, ext)) = token.rsplit_once('.') else {
        return false;
    };
    if ext.is_empty() || ext.len() > 6 {
        return false;
    }
    ext.chars().all(|c| c.is_ascii_alphanumeric())
}

impl QueryClassifier {
    #[must_use]
    pub fn classify(query: &str) -> QueryType {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return QueryType::Conceptual;
        }

        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if tokens.len() == 1 {
            let token = tokens[0];
            if Self::is_path_token(token) {
                return QueryType::Path;
            }
        } else {
            let path_tokens = tokens.iter().filter(|t| Self::is_path_token(t)).count();
            if path_tokens > 0 && (path_tokens == tokens.len() || tokens.len() <= 2) {
                return QueryType::Path;
            }
        }

        // Support mixed queries that start with an identifier and add clarifying words, e.g.
        // "touch_daemon_best_effort ttl". This is a common agent workflow and should behave
        // like identifier search rather than purely conceptual.
        if tokens.len() > 1 {
            let first = Self::strip_identifier_punct(tokens[0]);
            if !first.is_empty()
                && !Self::is_path_token(first)
                && !Self::is_question_leader(first)
                && Self::is_identifier_like(first)
            {
                return QueryType::Identifier;
            }
        }

        if Self::is_identifier_like(trimmed) {
            return QueryType::Identifier;
        }

        QueryType::Conceptual
    }

    /// Heuristic: does the query look like a request for documentation rather than implementation?
    ///
    /// This is used to pick safer defaults for agent workflows (e.g. code-first ranking vs
    /// docs-first ranking).
    #[must_use]
    pub fn is_docs_intent(query: &str) -> bool {
        let trimmed = query.trim();
        let q = trimmed.to_ascii_lowercase();
        if q.is_empty() {
            return false;
        }

        let ext = std::path::Path::new(trimmed)
            .extension()
            .and_then(|ext| ext.to_str());
        if ext.is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("mdx"))
        {
            return true;
        }

        [
            "readme",
            "docs",
            "documentation",
            "guide",
            "tutorial",
            "quick start",
            "usage",
            "install",
            "architecture",
            "rfc",
            "adr",
            "changelog",
            "license",
            "contributing",
            "philosophy",
        ]
        .iter()
        .any(|needle| q.contains(needle))
    }

    #[must_use]
    pub fn weights(query: &str) -> QueryWeights {
        match Self::classify(query) {
            // Exact/symbol queries should favor fuzzy matches for top-1 precision
            QueryType::Identifier => QueryWeights::new(0.1, 0.9, 3),
            QueryType::Path => QueryWeights::new(0.15, 0.85, 4),
            QueryType::Conceptual => {
                let words = query.split_whitespace().count();
                if words >= 4 {
                    QueryWeights::new(0.9, 0.1, 6)
                } else {
                    QueryWeights::new(0.8, 0.2, 6)
                }
            }
        }
    }

    fn is_path_token(token: &str) -> bool {
        let has_sep = token.contains('/') || token.contains('\\');
        let has_colons = token.contains("::");
        let has_ext = has_file_extension(token);
        has_sep || has_colons || has_ext
    }

    /// Heuristic: does the first token look like a natural-language question lead?
    ///
    /// This prevents false positives like `"How does X work"` being classified as Identifier
    /// just because "How" is mixed-case (capitalized) in English.
    fn is_question_leader(first_token: &str) -> bool {
        let lowered = first_token.trim().to_lowercase();
        matches!(
            lowered.as_str(),
            "how"
                | "what"
                | "why"
                | "where"
                | "when"
                | "who"
                | "which"
                | "does"
                | "do"
                | "is"
                | "are"
                | "can"
                | "could"
                | "should"
                | "will"
                | "would"
                | "explain"
                | "describe"
                | "tell"
                | "show"
                | "list"
                | "find"
                | "help"
                | "как"
                | "что"
                | "почему"
                | "где"
                | "когда"
                | "кто"
                | "какой"
                | "какая"
                | "какие"
                | "объясни"
                | "объясните"
                | "опиши"
                | "опишите"
                | "покажи"
                | "покажите"
                | "найди"
                | "найдите"
                | "перечисли"
                | "перечислите"
        )
    }

    fn is_identifier_like(query: &str) -> bool {
        if query.contains(' ') {
            return false;
        }

        let has_snake = query.contains('_');
        let has_digits = query.chars().any(|c| c.is_ascii_digit());
        let has_mixed_case = query.chars().any(|c| c.is_ascii_lowercase())
            && query.chars().any(|c| c.is_ascii_uppercase());

        has_snake || has_digits || has_mixed_case
    }

    fn strip_identifier_punct(token: &str) -> &str {
        token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != ':')
    }
}

#[cfg(test)]
mod tests {
    use super::{QueryClassifier, QueryType};

    #[test]
    fn classify_identifier() {
        assert_eq!(
            QueryClassifier::classify("HybridSearch"),
            QueryType::Identifier
        );
        assert_eq!(
            QueryClassifier::classify("chunk_metadata"),
            QueryType::Identifier
        );
        assert_eq!(
            QueryClassifier::classify("touch_daemon_best_effort ttl"),
            QueryType::Identifier
        );
    }

    #[test]
    fn classify_path() {
        assert_eq!(QueryClassifier::classify("src/lib.rs"), QueryType::Path);
        assert_eq!(
            QueryClassifier::classify("context::search"),
            QueryType::Path
        );
        assert_eq!(
            QueryClassifier::classify("crates/cli/src/lib.rs error"),
            QueryType::Path
        );
    }

    #[test]
    fn classify_conceptual() {
        assert_eq!(
            QueryClassifier::classify("async error handling"),
            QueryType::Conceptual
        );
        assert_eq!(
            QueryClassifier::classify("how does the MCP server load chunks for map/search"),
            QueryType::Conceptual
        );
        assert_eq!(
            QueryClassifier::classify("How does the MCP server load chunks for map/search"),
            QueryType::Conceptual
        );
        assert_eq!(
            QueryClassifier::classify(
                "Explain the decision logic for when the tool uses the semantic index versus the filesystem scan"
            ),
            QueryType::Conceptual
        );
    }

    #[test]
    fn weights_prioritize_exact_matches_for_identifiers() {
        let w_ident = QueryClassifier::weights("HybridSearch");
        assert!(w_ident.fuzzy > w_ident.semantic);
        assert!(w_ident.semantic <= 0.15);

        let w_path = QueryClassifier::weights("src/lib.rs");
        assert!(w_path.fuzzy > w_path.semantic);

        let w_concept_short = QueryClassifier::weights("error handling");
        assert!(w_concept_short.semantic > w_concept_short.fuzzy);
        let w_concept_long = QueryClassifier::weights("async error handling in parser");
        assert!(w_concept_long.semantic > w_concept_long.fuzzy);
    }

    #[test]
    fn docs_intent_detects_common_doc_queries() {
        assert!(QueryClassifier::is_docs_intent("README.md"));
        assert!(QueryClassifier::is_docs_intent("docs/ARCHITECTURE.md"));
        assert!(QueryClassifier::is_docs_intent("how to install"));
        assert!(!QueryClassifier::is_docs_intent("apexd"));
    }
}
