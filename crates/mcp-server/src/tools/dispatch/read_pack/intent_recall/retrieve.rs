use super::super::super::router::context_pack::context_pack;
use super::super::super::ContextPackRequest;
use super::super::anchor_scan::best_anchor_line_for_kind;
use super::super::candidates::{collect_ops_file_candidates, ops_candidate_score};
use super::super::cursors::snippet_kind_for_path;
use super::super::recall_directives::build_semantic_query;
use super::super::recall_directives::RecallQuestionMode;
use super::super::recall_keywords::best_keyword_pattern;
use super::super::recall_ops::ops_grep_pattern;
use super::super::recall_paths::recall_path_allowed;
use super::super::recall_scoring::{recall_has_code_snippet, score_recall_snippet};
use super::super::recall_snippets::{
    recall_upgrade_to_code_snippets, snippet_from_file, snippets_from_grep,
    snippets_from_grep_filtered, GrepSnippetParams, RecallCodeUpgradeParams, SnippetFromFileParams,
};
use super::super::recall_structural::recall_structural_candidates;
use super::super::{
    ContextFinderService, ProjectFactsResult, ReadPackContext, ReadPackSnippet,
    ReadPackSnippetKind, ResponseMode, REASON_HALO_CONTEXT_PACK_PRIMARY,
};
use super::question::RecallQuestionContext;
use std::collections::HashSet;

pub(super) async fn collect_recall_snippets(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    response_mode: ResponseMode,
    question: &RecallQuestionContext,
    topics: Option<&Vec<String>>,
    facts_snapshot: &ProjectFactsResult,
    used_files: &mut HashSet<String>,
) -> Vec<ReadPackSnippet> {
    let mut snippets: Vec<ReadPackSnippet> = Vec::new();

    if let Some((file, line)) = question.file_ref.clone() {
        if let Ok(snippet) = snippet_from_file(
            service,
            ctx,
            &file,
            SnippetFromFileParams {
                around_line: line,
                max_lines: question.snippet_max_lines,
                max_chars: question.snippet_max_chars,
                allow_secrets: question.allow_secrets,
            },
            response_mode,
        )
        .await
        {
            snippets.push(snippet);
        }
    }

    if snippets.is_empty() {
        if let Some(structural_intent) = question.structural_intent {
            let candidates =
                recall_structural_candidates(structural_intent, &ctx.root, facts_snapshot);
            for file in candidates.into_iter().take(32) {
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
                let anchor = best_anchor_line_for_kind(&ctx.root, &file, kind);

                if let Ok(snippet) = snippet_from_file(
                    service,
                    ctx,
                    &file,
                    SnippetFromFileParams {
                        around_line: anchor,
                        max_lines: question.snippet_max_lines,
                        max_chars: question.snippet_max_chars,
                        allow_secrets: question.allow_secrets,
                    },
                    response_mode,
                )
                .await
                {
                    snippets.push(snippet);
                }

                if snippets.len() >= question.snippet_limit {
                    break;
                }
            }
        }
    }

    if snippets.is_empty() {
        if let Some(regex) = question.regex_directive.as_deref() {
            if let Ok((found, _)) = snippets_from_grep_filtered(
                ctx,
                regex,
                GrepSnippetParams {
                    file: None,
                    file_pattern: question.effective_file_pattern.clone(),
                    before: question.grep_context_lines,
                    after: question.grep_context_lines,
                    max_hunks: question.snippet_limit,
                    max_chars: question.snippet_max_chars,
                    case_sensitive: true,
                    allow_secrets: question.allow_secrets,
                },
                &question.effective_include_paths,
                &question.effective_exclude_paths,
                question.effective_file_pattern.as_deref(),
            )
            .await
            {
                snippets = found;
            } else {
                let escaped = regex::escape(regex);
                if let Ok((found, _)) = snippets_from_grep_filtered(
                    ctx,
                    &escaped,
                    GrepSnippetParams {
                        file: None,
                        file_pattern: question.effective_file_pattern.clone(),
                        before: question.grep_context_lines,
                        after: question.grep_context_lines,
                        max_hunks: question.snippet_limit,
                        max_chars: question.snippet_max_chars,
                        case_sensitive: false,
                        allow_secrets: question.allow_secrets,
                    },
                    &question.effective_include_paths,
                    &question.effective_exclude_paths,
                    question.effective_file_pattern.as_deref(),
                )
                .await
                {
                    snippets = found;
                }
            }
        }
    }

    if snippets.is_empty() {
        if let Some(literal) = question.literal_directive.as_deref() {
            let escaped = regex::escape(literal);
            if let Ok((found, _)) = snippets_from_grep_filtered(
                ctx,
                &escaped,
                GrepSnippetParams {
                    file: None,
                    file_pattern: question.effective_file_pattern.clone(),
                    before: question.grep_context_lines,
                    after: question.grep_context_lines,
                    max_hunks: question.snippet_limit,
                    max_chars: question.snippet_max_chars,
                    case_sensitive: false,
                    allow_secrets: question.allow_secrets,
                },
                &question.effective_include_paths,
                &question.effective_exclude_paths,
                question.effective_file_pattern.as_deref(),
            )
            .await
            {
                snippets = found;
            }
        }
    }

    if snippets.is_empty() {
        if let Some(intent) = question.ops {
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
                        .then_with(|| {
                            ops_candidate_score(&b.file).cmp(&ops_candidate_score(&a.file))
                        })
                        .then_with(|| a.file.cmp(&b.file))
                        .then_with(|| a.start_line.cmp(&b.start_line))
                        .then_with(|| a.end_line.cmp(&b.end_line))
                });

                found_snippets.truncate(question.snippet_limit);
                snippets = found_snippets;
            }

            // If there are no concrete command matches, fall back to a deterministic
            // anchor-based doc snippet instead of grepping the entire repo.
            if snippets.is_empty() {
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
            }
        }
    }

    if snippets.is_empty() {
        // Best-effort: use semantic search if an index already exists; otherwise fall back to grep.
        let avoid_semantic_for_structural = question.structural_intent.is_some()
            && question.question_mode != RecallQuestionMode::Deep;
        let is_ops = question.ops.is_some();
        if question.allow_semantic
            && !avoid_semantic_for_structural
            && (!is_ops || question.question_mode == RecallQuestionMode::Deep)
        {
            let tool_result = context_pack(
                service,
                ContextPackRequest {
                    path: Some(ctx.root_display.clone()),
                    query: build_semantic_query(&question.clean_question, topics),
                    language: None,
                    strategy: None,
                    limit: Some(question.snippet_limit),
                    max_chars: Some(
                        question
                            .snippet_max_chars
                            .saturating_mul(question.snippet_limit)
                            .saturating_mul(2)
                            .clamp(1_000, 20_000),
                    ),
                    include_paths: if question.effective_include_paths.is_empty() {
                        None
                    } else {
                        Some(question.effective_include_paths.clone())
                    },
                    exclude_paths: if question.effective_exclude_paths.is_empty() {
                        None
                    } else {
                        Some(question.effective_exclude_paths.clone())
                    },
                    file_pattern: question.effective_file_pattern.clone(),
                    max_related_per_primary: Some(1),
                    include_docs: question.include_docs,
                    prefer_code: question.prefer_code,
                    related_mode: Some("focus".to_string()),
                    response_mode: Some(ResponseMode::Minimal),
                    trace: Some(false),
                    auto_index: None,
                    auto_index_budget_ms: None,
                },
            )
            .await;

            if let Ok(tool_result) = tool_result {
                if tool_result.is_error != Some(true) {
                    if let Some(value) = tool_result.structured_content.clone() {
                        if let Some(items) = value.get("items").and_then(|v| v.as_array()) {
                            for item in items.iter().take(question.snippet_limit) {
                                let Some(file) = item.get("file").and_then(|v| v.as_str()) else {
                                    continue;
                                };
                                let Some(content) = item.get("content").and_then(|v| v.as_str())
                                else {
                                    continue;
                                };
                                let start_line =
                                    item.get("start_line").and_then(|v| v.as_u64()).unwrap_or(1)
                                        as usize;
                                let start_line_u64 = start_line as u64;
                                let end_line = item
                                    .get("end_line")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(start_line_u64)
                                    as usize;
                                if !question.allow_secrets
                                    && super::super::candidates::is_disallowed_memory_file(file)
                                {
                                    continue;
                                }
                                snippets.push(ReadPackSnippet {
                                    file: file.to_string(),
                                    start_line,
                                    end_line,
                                    content: super::super::cursors::trim_chars(
                                        content,
                                        question.snippet_max_chars,
                                    ),
                                    kind: if response_mode == ResponseMode::Minimal {
                                        None
                                    } else {
                                        Some(snippet_kind_for_path(file))
                                    },
                                    reason: Some(REASON_HALO_CONTEXT_PACK_PRIMARY.to_string()),
                                    next_cursor: None,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    if snippets.is_empty() && question.ops.is_none() {
        if let Some(keyword) = best_keyword_pattern(&question.clean_question) {
            if let Ok((found, _)) = snippets_from_grep_filtered(
                ctx,
                &keyword,
                GrepSnippetParams {
                    file: None,
                    file_pattern: question.effective_file_pattern.clone(),
                    before: question.grep_context_lines,
                    after: question.grep_context_lines,
                    max_hunks: question.snippet_limit,
                    max_chars: question.snippet_max_chars,
                    case_sensitive: false,
                    allow_secrets: question.allow_secrets,
                },
                &question.effective_include_paths,
                &question.effective_exclude_paths,
                question.effective_file_pattern.as_deref(),
            )
            .await
            {
                snippets = found;
            }
        }
    }

    if question.effective_prefer_code
        && question.structural_intent.is_none()
        && question.ops.is_none()
        && !question.user_directive
        && !question.docs_intent
        && !snippets.is_empty()
        && !recall_has_code_snippet(&snippets)
    {
        let _ = recall_upgrade_to_code_snippets(
            RecallCodeUpgradeParams {
                ctx,
                facts_snapshot,
                question_tokens: &question.question_tokens,
                snippet_limit: question.snippet_limit,
                snippet_max_chars: question.snippet_max_chars,
                grep_context_lines: question.grep_context_lines,
                include_paths: &question.effective_include_paths,
                exclude_paths: &question.effective_exclude_paths,
                file_pattern: question.effective_file_pattern.as_deref(),
                allow_secrets: question.allow_secrets,
            },
            &mut snippets,
        )
        .await;
    }

    if snippets.len() > question.snippet_limit {
        snippets.truncate(question.snippet_limit);
    }

    // Global de-dupe: prefer covering *more files* (breadth) when answering multiple
    // questions in one call. This prevents "README spam" from consuming the entire budget.
    if snippets.len() > 1 {
        let mut unique: Vec<ReadPackSnippet> = Vec::new();
        let mut duplicates: Vec<ReadPackSnippet> = Vec::new();
        for snippet in snippets {
            if used_files.insert(snippet.file.clone()) {
                unique.push(snippet);
            } else {
                duplicates.push(snippet);
            }
        }
        if unique.is_empty() {
            if let Some(first) = duplicates.into_iter().next() {
                unique.push(first);
            }
        }
        snippets = unique;
    } else if let Some(snippet) = snippets.first() {
        used_files.insert(snippet.file.clone());
    }

    snippets
}
