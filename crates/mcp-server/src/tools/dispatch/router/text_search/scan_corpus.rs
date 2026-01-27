use super::budget::{estimate_match_cost, truncate_to_chars};
use super::types::{TextSearchOutcome, TextSearchSettings};

use crate::tools::dispatch::CallToolResult;
use crate::tools::dispatch::ContextFinderService;
use crate::tools::schemas::text_search::{TextSearchCursorModeV1, TextSearchMatch};
use context_protocol::BudgetTruncation;
use context_vector_store::ChunkCorpus;

use super::super::error::invalid_cursor;

pub(super) fn search_in_corpus(
    corpus: &ChunkCorpus,
    settings: &TextSearchSettings<'_>,
    content_max_chars: usize,
    start_file_index: usize,
    start_chunk_index: usize,
    start_line_offset: usize,
) -> std::result::Result<TextSearchOutcome, CallToolResult> {
    let mut outcome = TextSearchOutcome::new();

    let mut files: Vec<(&String, &Vec<context_code_chunker::CodeChunk>)> =
        corpus.files().iter().collect();
    files.sort_by(|a, b| a.0.cmp(b.0));
    files.retain(|(file, _)| {
        ContextFinderService::matches_file_pattern(file, settings.file_pattern)
    });

    if start_file_index > files.len() {
        return Err(invalid_cursor("Invalid cursor: out of range"));
    }

    'outer_corpus: for (file_index, (_file, chunks)) in
        files.iter().enumerate().skip(start_file_index)
    {
        if outcome.matches.len() >= settings.max_results {
            outcome.truncated = true;
            outcome.truncation = Some(BudgetTruncation::MaxMatches);
            outcome.next_state = Some(TextSearchCursorModeV1::Corpus {
                file_index,
                chunk_index: 0,
                line_offset: 0,
            });
            break 'outer_corpus;
        }

        outcome.scanned_files += 1;

        let mut chunk_refs: Vec<&context_code_chunker::CodeChunk> = chunks.iter().collect();
        chunk_refs.sort_by(|a, b| {
            a.start_line
                .cmp(&b.start_line)
                .then_with(|| a.end_line.cmp(&b.end_line))
        });

        let first_file = file_index == start_file_index;
        let start_chunk = if first_file { start_chunk_index } else { 0 };
        if start_chunk > chunk_refs.len() {
            return Err(invalid_cursor("Invalid cursor: out of range"));
        }

        for (chunk_index, chunk) in chunk_refs.iter().enumerate().skip(start_chunk) {
            if outcome.matches.len() >= settings.max_results {
                outcome.truncated = true;
                outcome.truncation = Some(BudgetTruncation::MaxMatches);
                outcome.next_state = Some(TextSearchCursorModeV1::Corpus {
                    file_index,
                    chunk_index,
                    line_offset: 0,
                });
                break 'outer_corpus;
            }

            let line_start = if first_file && chunk_index == start_chunk {
                start_line_offset
            } else {
                0
            };

            for (offset, line_text) in chunk.content.lines().enumerate().skip(line_start) {
                if outcome.matches.len() >= settings.max_results {
                    outcome.truncated = true;
                    outcome.truncation = Some(BudgetTruncation::MaxMatches);
                    outcome.next_state = Some(TextSearchCursorModeV1::Corpus {
                        file_index,
                        chunk_index,
                        line_offset: offset,
                    });
                    break 'outer_corpus;
                }

                let Some(col_byte) = ContextFinderService::match_in_line(
                    line_text,
                    settings.pattern,
                    settings.case_sensitive,
                    settings.whole_word,
                ) else {
                    continue;
                };

                let line = chunk.start_line + offset;
                let column = line_text[..col_byte].chars().count() + 1;
                let mut item = TextSearchMatch {
                    file: chunk.file_path.clone(),
                    line,
                    column,
                    text: line_text.to_string(),
                };

                let new_file = !outcome.matched_files.contains(&item.file);
                let cost = estimate_match_cost(&item.file, &item.text, new_file);
                if outcome.matches.is_empty() && cost > content_max_chars {
                    let file_chars = if new_file {
                        item.file.chars().count()
                    } else {
                        0
                    };
                    let fixed_overhead = 36usize + 24usize;
                    let allowed_text = content_max_chars
                        .saturating_sub(file_chars.saturating_add(fixed_overhead))
                        .max(1);
                    item.text = truncate_to_chars(&item.text, allowed_text);
                    let truncated_cost = estimate_match_cost(&item.file, &item.text, new_file);
                    if outcome.push_match(item) {
                        outcome.used_chars = outcome.used_chars.saturating_add(truncated_cost);
                    }
                    outcome.truncated = true;
                    outcome.truncation = Some(BudgetTruncation::MaxChars);
                    outcome.next_state = Some(TextSearchCursorModeV1::Corpus {
                        file_index,
                        chunk_index,
                        line_offset: offset.saturating_add(1),
                    });
                    break 'outer_corpus;
                }

                if outcome.used_chars.saturating_add(cost) > content_max_chars {
                    outcome.truncated = true;
                    outcome.truncation = Some(BudgetTruncation::MaxChars);
                    outcome.next_state = Some(TextSearchCursorModeV1::Corpus {
                        file_index,
                        chunk_index,
                        line_offset: offset,
                    });
                    break 'outer_corpus;
                }

                if outcome.push_match(item) {
                    outcome.used_chars = outcome.used_chars.saturating_add(cost);
                }
            }
        }
    }

    Ok(outcome)
}
