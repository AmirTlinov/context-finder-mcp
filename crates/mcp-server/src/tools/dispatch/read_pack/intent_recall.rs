use super::super::router::context_pack::context_pack;
use super::super::router::cursor_alias::compact_cursor_alias;
use super::super::{encode_cursor, ContextPackRequest};
use super::anchor_scan::best_anchor_line_for_kind;
use super::candidates::{
    collect_ops_file_candidates, is_disallowed_memory_file, ops_candidate_score,
};
use super::cursors::{
    normalize_optional_pattern, normalize_path_prefix_list, normalize_questions, normalize_topics,
    snippet_kind_for_path, trim_chars, trimmed_non_empty_str, ReadPackRecallCursorStoredV1,
    ReadPackRecallCursorV1, DEFAULT_RECALL_SNIPPETS_PER_QUESTION, MAX_RECALL_FILTER_PATHS,
    MAX_RECALL_SNIPPETS_PER_QUESTION,
};
use super::project_facts::compute_project_facts;
use super::recall::{extract_existing_file_ref, recall_structural_intent};
use super::recall_cursor::decode_recall_cursor;
use super::recall_directives::{
    build_semantic_query, parse_recall_literal_directive, parse_recall_regex_directive,
};
pub(super) use super::recall_directives::{
    parse_recall_question_directives, recall_question_policy, RecallQuestionMode,
};
pub(super) use super::recall_keywords::best_keyword_pattern;
use super::recall_keywords::recall_question_tokens;
use super::recall_ops::{ops_grep_pattern, ops_intent};
use super::recall_paths::{merge_recall_prefix_lists, recall_path_allowed};
use super::recall_scoring::{recall_has_code_snippet, score_recall_snippet};
use super::recall_snippets::{
    recall_upgrade_to_code_snippets, snippet_from_file, snippets_from_grep,
    RecallCodeUpgradeParams, SnippetFromFileParams,
};
pub(super) use super::recall_snippets::{snippets_from_grep_filtered, GrepSnippetParams};
use super::recall_structural::recall_structural_candidates;
use super::{
    call_error, invalid_cursor_with_meta_details, ContextFinderService, ReadPackContext,
    ReadPackRecallResult, ReadPackRequest, ReadPackSection, ReadPackSnippet, ReadPackSnippetKind,
    ResponseMode, ToolResult, CURSOR_VERSION, MAX_RECALL_INLINE_CURSOR_CHARS,
    REASON_HALO_CONTEXT_PACK_PRIMARY,
};
use crate::tools::cursor::cursor_fingerprint;
use context_indexer::{root_fingerprint, ToolMeta};
use context_search::QueryClassifier;
use serde_json::json;
use std::collections::HashSet;

pub(super) async fn handle_recall_intent(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    semantic_index_fresh: bool,
    sections: &mut Vec<ReadPackSection>,
    next_cursor_out: &mut Option<String>,
) -> ToolResult<()> {
    let (
        questions,
        topics,
        start_index,
        include_paths,
        exclude_paths,
        file_pattern,
        prefer_code,
        include_docs,
        allow_secrets,
    ) = if let Some(cursor) = trimmed_non_empty_str(request.cursor.as_deref()) {
        let overrides = request.ask.is_some()
            || request.questions.is_some()
            || request.topics.is_some()
            || request
                .include_paths
                .as_ref()
                .is_some_and(|p| p.iter().any(|p| !p.trim().is_empty()))
            || request
                .exclude_paths
                .as_ref()
                .is_some_and(|p| p.iter().any(|p| !p.trim().is_empty()))
            || trimmed_non_empty_str(request.file_pattern.as_deref()).is_some()
            || request.prefer_code.is_some()
            || request.include_docs.is_some()
            || request.allow_secrets.is_some();
        if overrides {
            return Err(call_error(
                "invalid_cursor",
                "Cursor continuation does not allow overriding recall parameters",
            ));
        }

        let decoded: ReadPackRecallCursorV1 = decode_recall_cursor(service, cursor).await?;
        if decoded.v != CURSOR_VERSION || decoded.tool != "read_pack" || decoded.mode != "recall" {
            return Err(call_error("invalid_cursor", "Invalid cursor: wrong tool"));
        }
        let expected_root_hash = cursor_fingerprint(&ctx.root_display);
        let expected_root_fingerprint = root_fingerprint(&ctx.root_display);
        if let Some(hash) = decoded.root_hash {
            if hash != expected_root_hash {
                return Err(invalid_cursor_with_meta_details(
                    "Invalid cursor: different root",
                    ToolMeta {
                        root_fingerprint: Some(expected_root_fingerprint),
                        ..ToolMeta::default()
                    },
                    json!({
                        "expected_root_fingerprint": expected_root_fingerprint,
                        "cursor_root_fingerprint": Some(hash),
                    }),
                ));
            }
        } else if decoded.root.as_deref() != Some(ctx.root_display.as_str()) {
            let cursor_root_fingerprint = decoded.root.as_deref().map(root_fingerprint);
            return Err(invalid_cursor_with_meta_details(
                "Invalid cursor: different root",
                ToolMeta {
                    root_fingerprint: Some(expected_root_fingerprint),
                    ..ToolMeta::default()
                },
                json!({
                    "expected_root_fingerprint": expected_root_fingerprint,
                    "cursor_root_fingerprint": cursor_root_fingerprint,
                }),
            ));
        }

        (
            decoded.questions,
            decoded.topics,
            decoded.next_question_index,
            decoded.include_paths,
            decoded.exclude_paths,
            decoded.file_pattern,
            decoded.prefer_code,
            decoded.include_docs,
            decoded.allow_secrets,
        )
    } else {
        (
            normalize_questions(request),
            normalize_topics(request),
            0,
            normalize_path_prefix_list(request.include_paths.as_ref()),
            normalize_path_prefix_list(request.exclude_paths.as_ref()),
            normalize_optional_pattern(request.file_pattern.as_deref()),
            request.prefer_code,
            request.include_docs,
            request.allow_secrets.unwrap_or(false),
        )
    };

    if questions.is_empty() {
        return Err(call_error(
            "missing_field",
            "Error: ask or questions is required for intent=recall",
        ));
    }

    let facts_snapshot = sections
        .iter()
        .find_map(|section| match section {
            ReadPackSection::ProjectFacts { result } => Some(result.clone()),
            _ => None,
        })
        .unwrap_or_else(|| compute_project_facts(&ctx.root));

    // Recall is a tight-loop tool and must stay cheap by default.
    //
    // Agent-native behavior: do not expose indexing knobs. Semantic retrieval is used only when
    // the index is already fresh, or when the user explicitly tags a question as `deep`.

    let remaining_questions = questions.len().saturating_sub(start_index).max(1);
    // Memory-UX heuristic: try to answer *more* questions per call by default, but keep snippets
    // small/dry so we fit under budget. This makes recall feel like "project memory" instead of
    // "a sequence of grep calls".
    //
    // We reserve a small slice for the facts section so the questions don't starve the front of
    // the page under mid budgets.
    let reserve_for_facts = match ctx.inner_max_chars {
        0..=2_000 => 260,
        2_001..=6_000 => 420,
        6_001..=12_000 => 650,
        _ => 900,
    };
    let recall_budget_pool = ctx
        .inner_max_chars
        .saturating_sub(reserve_for_facts)
        .max(80)
        .min(ctx.inner_max_chars);

    // Target ~1.4k chars per question under `.context` output. This is intentionally conservative:
    // we'd rather answer more questions with smaller snippets and let the agent "zoom in" with
    // cursor/deep mode.
    let target_per_question = 1_400usize;
    let min_per_question = 650usize;

    let max_questions_by_target = (recall_budget_pool / target_per_question).clamp(1, 8);
    let max_questions_by_min = (recall_budget_pool / min_per_question).max(1);
    let max_questions_this_call = max_questions_by_target
        .min(max_questions_by_min)
        .min(remaining_questions);

    let per_question_budget = recall_budget_pool
        .saturating_div(max_questions_this_call.max(1))
        .max(80);

    // Under smaller per-question budgets, prefer fewer, more informative snippets.
    let default_snippets_auto = if per_question_budget < 1_500 {
        1
    } else if per_question_budget < 3_200 {
        2
    } else {
        DEFAULT_RECALL_SNIPPETS_PER_QUESTION
    };
    let default_snippets_fast = if per_question_budget < 1_500 { 1 } else { 2 };

    let mut used_files: HashSet<String> = {
        // Per-session working set: avoid repeating the same anchor files across multiple recall
        // calls in one agent session.
        let session = service.session.lock().await;
        session.seen_snippet_files_set_snapshot()
    };
    let mut processed = 0usize;
    let mut next_index = None;

    for (offset, question) in questions.iter().enumerate().skip(start_index) {
        let mut snippets: Vec<ReadPackSnippet> = Vec::new();

        let (clean_question, directives) = parse_recall_question_directives(question, &ctx.root);
        let clean_question = if clean_question.is_empty() {
            question.clone()
        } else {
            clean_question
        };
        let user_directive = parse_recall_regex_directive(&clean_question).is_some()
            || parse_recall_literal_directive(&clean_question).is_some();
        let structural_intent = if user_directive {
            None
        } else {
            recall_structural_intent(&clean_question)
        };
        let ops = ops_intent(&clean_question);
        let is_ops = ops.is_some();
        let question_tokens = recall_question_tokens(&clean_question);

        let docs_intent = QueryClassifier::is_docs_intent(&clean_question);
        let effective_prefer_code = prefer_code.unwrap_or(!docs_intent);

        let question_mode = directives.mode;
        let base_snippet_limit = match question_mode {
            RecallQuestionMode::Fast => default_snippets_fast,
            RecallQuestionMode::Deep => MAX_RECALL_SNIPPETS_PER_QUESTION,
            RecallQuestionMode::Auto => default_snippets_auto,
        };
        let snippet_limit = directives
            .snippet_limit
            .unwrap_or(base_snippet_limit)
            .clamp(1, MAX_RECALL_SNIPPETS_PER_QUESTION);
        let grep_context_lines = directives.grep_context.unwrap_or(12);

        let snippet_max_chars = per_question_budget
            .saturating_div(snippet_limit.max(1))
            .clamp(40, 4_000)
            .min(ctx.inner_max_chars);
        let snippet_max_chars = match question_mode {
            RecallQuestionMode::Deep => snippet_max_chars,
            _ => snippet_max_chars.min(1_200),
        };
        let snippet_max_lines = if snippet_max_chars < 600 {
            60
        } else if snippet_max_chars < 1_200 {
            90
        } else {
            120
        };

        let policy = recall_question_policy(question_mode, semantic_index_fresh);
        let allow_semantic = policy.allow_semantic;

        let effective_include_paths = merge_recall_prefix_lists(
            &include_paths,
            &directives.include_paths,
            MAX_RECALL_FILTER_PATHS,
        );
        let effective_exclude_paths = merge_recall_prefix_lists(
            &exclude_paths,
            &directives.exclude_paths,
            MAX_RECALL_FILTER_PATHS,
        );

        let effective_file_pattern = directives
            .file_pattern
            .clone()
            .or_else(|| file_pattern.clone());

        let explicit_file_ref = directives.file_ref.clone();
        let detected_file_ref =
            extract_existing_file_ref(&clean_question, &ctx.root, allow_secrets);
        let file_ref = explicit_file_ref.or(detected_file_ref);

        if let Some((file, line)) = file_ref {
            if let Ok(snippet) = snippet_from_file(
                service,
                ctx,
                &file,
                SnippetFromFileParams {
                    around_line: line,
                    max_lines: snippet_max_lines,
                    max_chars: snippet_max_chars,
                    allow_secrets,
                },
                response_mode,
            )
            .await
            {
                snippets.push(snippet);
            }
        }

        if snippets.is_empty() {
            if let Some(structural_intent) = structural_intent {
                let candidates =
                    recall_structural_candidates(structural_intent, &ctx.root, &facts_snapshot);
                for file in candidates.into_iter().take(32) {
                    if !recall_path_allowed(
                        &file,
                        &effective_include_paths,
                        &effective_exclude_paths,
                    ) {
                        continue;
                    }
                    if !ContextFinderService::matches_file_pattern(
                        &file,
                        effective_file_pattern.as_deref(),
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
                            max_lines: snippet_max_lines,
                            max_chars: snippet_max_chars,
                            allow_secrets,
                        },
                        response_mode,
                    )
                    .await
                    {
                        snippets.push(snippet);
                    }

                    if snippets.len() >= snippet_limit {
                        break;
                    }
                }
            }
        }

        if snippets.is_empty() {
            if let Some(regex) = parse_recall_regex_directive(&clean_question) {
                if let Ok((found, _)) = snippets_from_grep_filtered(
                    ctx,
                    &regex,
                    GrepSnippetParams {
                        file: None,
                        file_pattern: effective_file_pattern.clone(),
                        before: grep_context_lines,
                        after: grep_context_lines,
                        max_hunks: snippet_limit,
                        max_chars: snippet_max_chars,
                        case_sensitive: true,
                        allow_secrets,
                    },
                    &effective_include_paths,
                    &effective_exclude_paths,
                    effective_file_pattern.as_deref(),
                )
                .await
                {
                    snippets = found;
                } else {
                    let escaped = regex::escape(&regex);
                    if let Ok((found, _)) = snippets_from_grep_filtered(
                        ctx,
                        &escaped,
                        GrepSnippetParams {
                            file: None,
                            file_pattern: effective_file_pattern.clone(),
                            before: grep_context_lines,
                            after: grep_context_lines,
                            max_hunks: snippet_limit,
                            max_chars: snippet_max_chars,
                            case_sensitive: false,
                            allow_secrets,
                        },
                        &effective_include_paths,
                        &effective_exclude_paths,
                        effective_file_pattern.as_deref(),
                    )
                    .await
                    {
                        snippets = found;
                    }
                }
            }
        }

        if snippets.is_empty() {
            if let Some(literal) = parse_recall_literal_directive(&clean_question) {
                let escaped = regex::escape(&literal);
                if let Ok((found, _)) = snippets_from_grep_filtered(
                    ctx,
                    &escaped,
                    GrepSnippetParams {
                        file: None,
                        file_pattern: effective_file_pattern.clone(),
                        before: grep_context_lines,
                        after: grep_context_lines,
                        max_hunks: snippet_limit,
                        max_chars: snippet_max_chars,
                        case_sensitive: false,
                        allow_secrets,
                    },
                    &effective_include_paths,
                    &effective_exclude_paths,
                    effective_file_pattern.as_deref(),
                )
                .await
                {
                    snippets = found;
                }
            }
        }

        if snippets.is_empty() {
            if let Some(intent) = ops {
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
                        &effective_include_paths,
                        &effective_exclude_paths,
                    ) {
                        continue;
                    }
                    if !ContextFinderService::matches_file_pattern(
                        &file,
                        effective_file_pattern.as_deref(),
                    ) {
                        continue;
                    }

                    let Ok((mut found, _)) = snippets_from_grep(
                        ctx,
                        pattern,
                        GrepSnippetParams {
                            file: Some(file.clone()),
                            file_pattern: None,
                            before: grep_context_lines.min(20),
                            after: grep_context_lines.min(20),
                            max_hunks: snippet_limit,
                            max_chars: snippet_max_chars,
                            case_sensitive: false,
                            allow_secrets,
                        },
                    )
                    .await
                    else {
                        continue;
                    };
                    found_snippets.append(&mut found);
                    if found_snippets.len() >= snippet_limit.saturating_mul(3) {
                        break;
                    }
                }

                if !found_snippets.is_empty() {
                    found_snippets.sort_by(|a, b| {
                        let a_score = score_recall_snippet(&question_tokens, a);
                        let b_score = score_recall_snippet(&question_tokens, b);
                        b_score
                            .cmp(&a_score)
                            .then_with(|| {
                                ops_candidate_score(&b.file).cmp(&ops_candidate_score(&a.file))
                            })
                            .then_with(|| a.file.cmp(&b.file))
                            .then_with(|| a.start_line.cmp(&b.start_line))
                            .then_with(|| a.end_line.cmp(&b.end_line))
                    });

                    found_snippets.truncate(snippet_limit);
                    snippets = found_snippets;
                }

                // If there are no concrete command matches, fall back to a deterministic
                // anchor-based doc snippet instead of grepping the entire repo.
                if snippets.is_empty() {
                    let candidates = collect_ops_file_candidates(&ctx.root);
                    for file in candidates.into_iter().take(10) {
                        if !recall_path_allowed(
                            &file,
                            &effective_include_paths,
                            &effective_exclude_paths,
                        ) {
                            continue;
                        }
                        if !ContextFinderService::matches_file_pattern(
                            &file,
                            effective_file_pattern.as_deref(),
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
                                max_lines: snippet_max_lines,
                                max_chars: snippet_max_chars,
                                allow_secrets,
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
            let avoid_semantic_for_structural =
                structural_intent.is_some() && question_mode != RecallQuestionMode::Deep;
            if allow_semantic
                && !avoid_semantic_for_structural
                && (!is_ops || question_mode == RecallQuestionMode::Deep)
            {
                let tool_result = context_pack(
                    service,
                    ContextPackRequest {
                        path: Some(ctx.root_display.clone()),
                        query: build_semantic_query(&clean_question, topics.as_ref()),
                        language: None,
                        strategy: None,
                        limit: Some(snippet_limit),
                        max_chars: Some(
                            snippet_max_chars
                                .saturating_mul(snippet_limit)
                                .saturating_mul(2)
                                .clamp(1_000, 20_000),
                        ),
                        include_paths: if effective_include_paths.is_empty() {
                            None
                        } else {
                            Some(effective_include_paths.clone())
                        },
                        exclude_paths: if effective_exclude_paths.is_empty() {
                            None
                        } else {
                            Some(effective_exclude_paths.clone())
                        },
                        file_pattern: effective_file_pattern.clone(),
                        max_related_per_primary: Some(1),
                        include_docs,
                        prefer_code,
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
                                for item in items.iter().take(snippet_limit) {
                                    let Some(file) = item.get("file").and_then(|v| v.as_str())
                                    else {
                                        continue;
                                    };
                                    let Some(content) =
                                        item.get("content").and_then(|v| v.as_str())
                                    else {
                                        continue;
                                    };
                                    let start_line = item
                                        .get("start_line")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(1)
                                        as usize;
                                    let start_line_u64 = start_line as u64;
                                    let end_line = item
                                        .get("end_line")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(start_line_u64)
                                        as usize;
                                    if !allow_secrets && is_disallowed_memory_file(file) {
                                        continue;
                                    }
                                    snippets.push(ReadPackSnippet {
                                        file: file.to_string(),
                                        start_line,
                                        end_line,
                                        content: trim_chars(content, snippet_max_chars),
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

        if snippets.is_empty() && !is_ops {
            if let Some(keyword) = best_keyword_pattern(&clean_question) {
                if let Ok((found, _)) = snippets_from_grep_filtered(
                    ctx,
                    &keyword,
                    GrepSnippetParams {
                        file: None,
                        file_pattern: effective_file_pattern.clone(),
                        before: grep_context_lines,
                        after: grep_context_lines,
                        max_hunks: snippet_limit,
                        max_chars: snippet_max_chars,
                        case_sensitive: false,
                        allow_secrets,
                    },
                    &effective_include_paths,
                    &effective_exclude_paths,
                    effective_file_pattern.as_deref(),
                )
                .await
                {
                    snippets = found;
                }
            }
        }

        if effective_prefer_code
            && structural_intent.is_none()
            && !is_ops
            && !user_directive
            && !docs_intent
            && !snippets.is_empty()
            && !recall_has_code_snippet(&snippets)
        {
            let _ = recall_upgrade_to_code_snippets(
                RecallCodeUpgradeParams {
                    ctx,
                    facts_snapshot: &facts_snapshot,
                    question_tokens: &question_tokens,
                    snippet_limit,
                    snippet_max_chars,
                    grep_context_lines,
                    include_paths: &effective_include_paths,
                    exclude_paths: &effective_exclude_paths,
                    file_pattern: effective_file_pattern.as_deref(),
                    allow_secrets,
                },
                &mut snippets,
            )
            .await;
        }

        if snippets.len() > snippet_limit {
            snippets.truncate(snippet_limit);
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

        sections.push(ReadPackSection::Recall {
            result: ReadPackRecallResult {
                question: question.clone(),
                snippets,
            },
        });
        processed += 1;

        // Pagination guard: keep recall bounded, while letting larger budgets answer more questions.
        if processed >= max_questions_this_call {
            next_index = Some(offset + 1);
            break;
        }
    }

    if let Some(next_question_index) = next_index {
        let remaining_questions: Vec<String> = questions
            .iter()
            .skip(next_question_index)
            .cloned()
            .collect();
        if remaining_questions.is_empty() {
            return Ok(());
        }
        let cursor = ReadPackRecallCursorV1 {
            v: CURSOR_VERSION,
            tool: "read_pack".to_string(),
            mode: "recall".to_string(),
            root: Some(ctx.root_display.clone()),
            root_hash: Some(cursor_fingerprint(&ctx.root_display)),
            max_chars: Some(ctx.max_chars),
            response_mode: Some(response_mode),
            questions: remaining_questions,
            topics,
            include_paths,
            exclude_paths,
            file_pattern,
            prefer_code,
            include_docs,
            allow_secrets,
            next_question_index: 0,
        };

        // Try to keep cursors inline (stateless) when small; otherwise store the full continuation
        // server-side and return a tiny cursor token (agent-friendly, avoids blowing context).
        if let Ok(token) = encode_cursor(&cursor) {
            if token.len() <= MAX_RECALL_INLINE_CURSOR_CHARS {
                *next_cursor_out = Some(compact_cursor_alias(service, token).await);
                return Ok(());
            }
        }

        let stored_bytes =
            serde_json::to_vec(&cursor).map_err(|err| call_error("internal", err.to_string()))?;
        let store_id = service.state.cursor_store_put(stored_bytes).await;
        let stored_cursor = ReadPackRecallCursorStoredV1 {
            v: CURSOR_VERSION,
            tool: "read_pack".to_string(),
            mode: "recall".to_string(),
            root: Some(ctx.root_display.clone()),
            root_hash: Some(cursor_fingerprint(&ctx.root_display)),
            max_chars: Some(ctx.max_chars),
            response_mode: Some(response_mode),
            store_id,
        };
        if let Ok(token) = encode_cursor(&stored_cursor) {
            *next_cursor_out = Some(compact_cursor_alias(service, token).await);
        }
    }

    Ok(())
}
