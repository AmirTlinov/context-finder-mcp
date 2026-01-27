use crate::tools::schemas::text_search::{TextSearchCursorModeV1, TextSearchMatch};
use context_protocol::BudgetTruncation;
use std::collections::HashSet;

pub(super) struct TextSearchSettings<'a> {
    pub(super) pattern: &'a str,
    pub(super) file_pattern: Option<&'a str>,
    pub(super) max_results: usize,
    pub(super) max_chars: usize,
    pub(super) case_sensitive: bool,
    pub(super) whole_word: bool,
}

pub(super) struct TextSearchOutcome {
    pub(super) matches: Vec<TextSearchMatch>,
    pub(super) matched_files: HashSet<String>,
    pub(super) scanned_files: usize,
    pub(super) skipped_large_files: usize,
    pub(super) truncated: bool,
    pub(super) truncation: Option<BudgetTruncation>,
    pub(super) used_chars: usize,
    pub(super) next_state: Option<TextSearchCursorModeV1>,

    seen: HashSet<TextSearchKey>,
}

#[derive(Hash, PartialEq, Eq)]
struct TextSearchKey {
    file: String,
    line: usize,
    column: usize,
    text: String,
}

impl TextSearchOutcome {
    pub(super) fn new() -> Self {
        Self {
            matches: Vec::new(),
            matched_files: HashSet::new(),
            seen: HashSet::new(),
            scanned_files: 0,
            skipped_large_files: 0,
            truncated: false,
            truncation: None,
            used_chars: 0,
            next_state: None,
        }
    }

    pub(super) fn push_match(&mut self, item: TextSearchMatch) -> bool {
        let key = TextSearchKey {
            file: item.file.clone(),
            line: item.line,
            column: item.column,
            text: item.text.clone(),
        };
        if !self.seen.insert(key) {
            return false;
        }
        self.matched_files.insert(item.file.clone());
        self.matches.push(item);
        true
    }
}
