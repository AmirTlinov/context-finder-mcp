use regex::escape;

pub(super) fn best_keyword_pattern(question: &str) -> Option<String> {
    let mut best: Option<String> = None;
    for token in question
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .filter(|t| t.len() >= 3)
    {
        if token.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let lowered = token.to_lowercase();
        if matches!(
            lowered.as_str(),
            "the"
                | "and"
                | "with"
                | "for"
                | "from"
                | "that"
                | "this"
                | "как"
                | "что"
                | "где"
                | "чем"
                | "когда"
                | "почему"
                | "который"
                | "которая"
                | "которые"
        ) {
            continue;
        }
        let replace = match best.as_ref() {
            None => true,
            Some(current) => token.len() > current.len(),
        };
        if replace {
            best = Some(token.to_string());
        }
    }
    best.map(|kw| escape(&kw))
}

pub(super) fn recall_question_tokens(question: &str) -> Vec<String> {
    // Deterministic, Unicode-friendly tokenization for lightweight relevance scoring.
    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();

    let flush = |out: &mut Vec<String>, buf: &mut String| {
        if buf.is_empty() {
            return;
        }
        let token = buf.to_lowercase();
        buf.clear();

        if token.len() < 3 {
            return;
        }
        if token.chars().all(|c| c.is_ascii_digit()) {
            return;
        }
        if matches!(
            token.as_str(),
            "the"
                | "and"
                | "with"
                | "for"
                | "from"
                | "that"
                | "this"
                | "как"
                | "что"
                | "где"
                | "чем"
                | "когда"
                | "почему"
                | "который"
                | "которая"
                | "которые"
                | "зачем"
                | "есть"
        ) {
            return;
        }
        out.push(token);
    };

    for ch in question.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            buf.push(ch);
            continue;
        }
        flush(&mut out, &mut buf);
        if out.len() >= 12 {
            break;
        }
    }
    flush(&mut out, &mut buf);

    out
}

pub(super) fn recall_keyword_patterns(question_tokens: &[String]) -> Vec<String> {
    let mut tokens: Vec<String> = question_tokens.to_vec();
    tokens.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    tokens.dedup();

    let mut out = Vec::new();
    for token in tokens {
        if token.len() < 3 {
            continue;
        }
        if out.iter().any(|p: &String| p == &token) {
            continue;
        }
        out.push(escape(&token));
        if out.len() >= 2 {
            break;
        }
    }
    out
}
