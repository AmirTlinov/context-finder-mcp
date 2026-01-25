use super::super::anchor_scan::best_anchor_line_for_kind;
use super::super::candidates::{collect_ops_file_candidates, ops_candidate_score};
use super::super::cursors::snippet_kind_for_path;
use super::super::recall_ops::ops_grep_pattern;
use super::super::recall_paths::recall_path_allowed;
use super::super::recall_scoring::score_recall_snippet;
use super::super::recall_snippets::{
    snippet_from_file, snippets_from_grep, GrepSnippetParams, SnippetFromFileParams,
};
use super::super::{
    ContextFinderService, ReadPackContext, ReadPackSnippet, ReadPackSnippetKind, ResponseMode,
};
use super::question::RecallQuestionContext;

pub(super) async fn ops_snippets(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    response_mode: ResponseMode,
    question: &RecallQuestionContext,
) -> Vec<ReadPackSnippet> {
    let Some(intent) = question.ops else {
        return Vec::new();
    };

    let pattern = ops_grep_pattern(intent);
    let candidates = collect_ops_file_candidates(&ctx.root);

    // Scan a bounded set of likely "commands live here" files and rerank matches by
    // overlap with the question. This avoids getting stuck on the first generic
    // `cargo run` mention when the question is actually about a more specific workflow
    // (e.g., golden snapshots).
    let mut found_snippets: Vec<ReadPackSnippet> = Vec::new();
    for file in candidates.into_iter().take(24) {
        if !recall_path_allowed(
            &file,
            &question.effective_include_paths,
            &question.effective_exclude_paths,
        ) {
            continue;
        }
        if !ContextFinderService::matches_file_pattern(
            &file,
            question.effective_file_pattern.as_deref(),
        ) {
            continue;
        }

        let Ok((mut found, _)) = snippets_from_grep(
            ctx,
            pattern,
            GrepSnippetParams {
                file: Some(file.clone()),
                file_pattern: None,
                before: question.grep_context_lines.min(20),
                after: question.grep_context_lines.min(20),
                max_hunks: question.snippet_limit,
                max_chars: question.snippet_max_chars,
                case_sensitive: false,
                allow_secrets: question.allow_secrets,
            },
        )
        .await
        else {
            continue;
        };
        found_snippets.append(&mut found);
        if found_snippets.len() >= question.snippet_limit.saturating_mul(3) {
            break;
        }
    }

    if !found_snippets.is_empty() {
        found_snippets.sort_by(|a, b| {
            let a_score = score_recall_snippet(&question.question_tokens, a);
            let b_score = score_recall_snippet(&question.question_tokens, b);
            b_score
                .cmp(&a_score)
                .then_with(|| ops_candidate_score(&b.file).cmp(&ops_candidate_score(&a.file)))
                .then_with(|| a.file.cmp(&b.file))
                .then_with(|| a.start_line.cmp(&b.start_line))
                .then_with(|| a.end_line.cmp(&b.end_line))
        });

        found_snippets.truncate(question.snippet_limit);
        return found_snippets;
    }

    // If there are no concrete command matches, fall back to a deterministic
    // anchor-based doc snippet instead of grepping the entire repo.
    let mut snippets = Vec::new();
    let candidates = collect_ops_file_candidates(&ctx.root);
    for file in candidates.into_iter().take(10) {
        if !recall_path_allowed(
            &file,
            &question.effective_include_paths,
            &question.effective_exclude_paths,
        ) {
            continue;
        }
        if !ContextFinderService::matches_file_pattern(
            &file,
            question.effective_file_pattern.as_deref(),
        ) {
            continue;
        }
        let kind = snippet_kind_for_path(&file);
        if kind == ReadPackSnippetKind::Code {
            continue;
        }
        let Some(anchor) = best_anchor_line_for_kind(&ctx.root, &file, kind) else {
            continue;
        };
        if let Ok(snippet) = snippet_from_file(
            service,
            ctx,
            &file,
            SnippetFromFileParams {
                around_line: Some(anchor),
                max_lines: question.snippet_max_lines,
                max_chars: question.snippet_max_chars,
                allow_secrets: question.allow_secrets,
            },
            response_mode,
        )
        .await
        {
            snippets.push(snippet);
            break;
        }
    }

    snippets
}
