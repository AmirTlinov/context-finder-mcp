use std::collections::HashMap;

/// Query expander for code search with domain-specific synonyms
pub struct QueryExpander {
    /// Synonym dictionary: term -> [synonyms]
    synonyms: HashMap<String, Vec<String>>,
}

impl QueryExpander {
    /// Create new query expander with built-in code synonyms
    pub fn new() -> Self {
        let mut synonyms = HashMap::new();

        // Error handling
        synonyms.insert(
            "error".to_string(),
            vec!["Error", "Err", "Result", "err", "failure", "exception"]
                .into_iter()
                .map(String::from)
                .collect(),
        );
        synonyms.insert(
            "handle".to_string(),
            vec!["handler", "handling", "process", "manage"]
                .into_iter()
                .map(String::from)
                .collect(),
        );
        synonyms.insert(
            "handling".to_string(),
            vec!["handler", "handle", "process", "manage"]
                .into_iter()
                .map(String::from)
                .collect(),
        );

        // Vector/embedding operations
        synonyms.insert(
            "vector".to_string(),
            vec!["embedding", "vec", "array", "tensor"]
                .into_iter()
                .map(String::from)
                .collect(),
        );
        synonyms.insert(
            "similarity".to_string(),
            vec!["distance", "cosine", "dot_product", "score"]
                .into_iter()
                .map(String::from)
                .collect(),
        );
        synonyms.insert(
            "embedding".to_string(),
            vec!["vector", "encode", "representation", "embed"]
                .into_iter()
                .map(String::from)
                .collect(),
        );

        // Search operations
        synonyms.insert(
            "search".to_string(),
            vec!["find", "query", "lookup", "retrieve"]
                .into_iter()
                .map(String::from)
                .collect(),
        );
        synonyms.insert(
            "index".to_string(),
            vec!["indexing", "store", "build", "create"]
                .into_iter()
                .map(String::from)
                .collect(),
        );

        // Code structure
        synonyms.insert(
            "function".to_string(),
            vec!["fn", "func", "method", "procedure"]
                .into_iter()
                .map(String::from)
                .collect(),
        );
        synonyms.insert(
            "class".to_string(),
            vec!["struct", "type", "object", "impl"]
                .into_iter()
                .map(String::from)
                .collect(),
        );
        synonyms.insert(
            "method".to_string(),
            vec!["function", "fn", "func", "member"]
                .into_iter()
                .map(String::from)
                .collect(),
        );

        // AST/parsing
        synonyms.insert(
            "parse".to_string(),
            vec!["parser", "parsing", "analyze", "AST", "tree"]
                .into_iter()
                .map(String::from)
                .collect(),
        );
        synonyms.insert(
            "ast".to_string(),
            vec!["tree", "syntax", "parse", "parser", "node"]
                .into_iter()
                .map(String::from)
                .collect(),
        );

        // Chunking/splitting
        synonyms.insert(
            "chunk".to_string(),
            vec!["split", "segment", "block", "part", "piece"]
                .into_iter()
                .map(String::from)
                .collect(),
        );

        // Fuzzy/string matching
        synonyms.insert(
            "fuzzy".to_string(),
            vec!["approximate", "similarity", "match", "typo"]
                .into_iter()
                .map(String::from)
                .collect(),
        );

        Self { synonyms }
    }

    /// Expand query with synonyms and variants
    pub fn expand(&self, query: &str) -> Vec<String> {
        let mut expansions = vec![query.to_string()];

        // Tokenize query (split by space, underscore, camelCase)
        let tokens = self.tokenize(query);

        // Add each token
        for token in &tokens {
            if !expansions.contains(&token.to_lowercase()) {
                expansions.push(token.to_lowercase());
            }
        }

        // Add synonyms for each token
        for token in &tokens {
            let token_lower = token.to_lowercase();
            if let Some(syns) = self.synonyms.get(&token_lower) {
                for syn in syns {
                    if !expansions.contains(syn) {
                        expansions.push(syn.clone());
                    }
                }
            }
        }

        // Limit expansion to avoid too many variants
        expansions.truncate(15);

        expansions
    }

    /// Tokenize query into words
    /// Handles: spaces, underscores, camelCase, PascalCase
    fn tokenize(&self, query: &str) -> Vec<String> {
        let mut tokens = Vec::new();

        // Split by space and underscore
        for word in query.split(|c: char| c.is_whitespace() || c == '_') {
            if word.is_empty() {
                continue;
            }

            // Split camelCase/PascalCase
            let camel_tokens = self.split_camel_case(word);
            tokens.extend(camel_tokens);
        }

        tokens
    }

    /// Split camelCase or PascalCase into words
    fn split_camel_case(&self, word: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut prev_upper = false;

        for ch in word.chars() {
            if ch.is_uppercase() {
                if !current.is_empty() && !prev_upper {
                    tokens.push(current.clone());
                    current.clear();
                }
                current.push(ch);
                prev_upper = true;
            } else {
                current.push(ch);
                prev_upper = false;
            }
        }

        if !current.is_empty() {
            tokens.push(current);
        }

        // If no split happened, return original
        if tokens.is_empty() {
            tokens.push(word.to_string());
        }

        tokens
    }

    /// Expand query and create weighted query string
    /// Format: "term1 term2 synonym1 synonym2..."
    pub fn expand_to_query(&self, query: &str) -> String {
        let expansions = self.expand(query);
        expansions.join(" ")
    }
}

impl Default for QueryExpander {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize() {
        let expander = QueryExpander::new();

        let tokens = expander.tokenize("error handling");
        assert_eq!(tokens, vec!["error", "handling"]);

        let tokens = expander.tokenize("error_handling");
        assert_eq!(tokens, vec!["error", "handling"]);

        let tokens = expander.tokenize("errorHandling");
        assert_eq!(tokens, vec!["error", "Handling"]);

        let tokens = expander.tokenize("ErrorHandling");
        assert_eq!(tokens, vec!["Error", "Handling"]);
    }

    #[test]
    fn test_expand() {
        let expander = QueryExpander::new();

        let expansions = expander.expand("error handling");
        assert!(expansions.contains(&"error handling".to_string()));
        assert!(expansions.contains(&"error".to_string()));
        assert!(expansions.contains(&"Result".to_string()));
        assert!(expansions.contains(&"handler".to_string()));
    }

    #[test]
    fn test_expand_camel_case() {
        let expander = QueryExpander::new();

        let expansions = expander.expand("cosineSimilarity");
        assert!(expansions.contains(&"cosine".to_string()));
        assert!(expansions.contains(&"similarity".to_string()));
    }

    #[test]
    fn test_expand_to_query() {
        let expander = QueryExpander::new();

        let expanded = expander.expand_to_query("error handling");
        assert!(expanded.contains("error"));
        assert!(expanded.contains("Result") || expanded.contains("Err"));
    }
}
