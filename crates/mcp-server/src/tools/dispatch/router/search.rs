use super::super::{
    tokenize_focus_query, AutoIndexPolicy, CallToolResult, Content, ContextFinderService, McpError,
    QueryClassifier, QueryType, ResponseMode, SearchRequest, SearchResponse, SearchResult,
    ToolMeta,
};
use crate::tools::context_doc::ContextDocBuilder;

use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_request_with_meta,
    meta_for_request,
};
use super::semantic_fallback::{grep_fallback_hunks, is_semantic_unavailable_error};
use context_protocol::ToolNextAction;

fn choose_anchor_token(tokens: &[String]) -> Option<String> {
    fn is_low_value(token_lc: &str) -> bool {
        matches!(
            token_lc,
            "struct"
                | "definition"
                | "define"
                | "defined"
                | "fn"
                | "function"
                | "method"
                | "class"
                | "type"
                | "enum"
                | "trait"
                | "impl"
                | "module"
                | "file"
                | "path"
                | "usage"
                | "usages"
                | "reference"
                | "references"
                | "where"
                | "find"
                | "show"
        )
    }

    tokens
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .filter(|t| t.len() >= 4)
        .filter(|t| !is_low_value(&t.to_lowercase()))
        .max_by_key(|t| t.len())
        .map(|t| t.to_string())
}

fn results_mention_token(results: &[SearchResult], token: &str) -> bool {
    let token = token.trim();
    if token.is_empty() {
        return true;
    }
    let token_lc = token.to_lowercase();
    results.iter().take(6).any(|r| {
        r.file.contains(token)
            || r.file.to_lowercase().contains(&token_lc)
            || r.symbol.as_deref().is_some_and(|s| {
                s.eq_ignore_ascii_case(token) || s.to_lowercase().contains(&token_lc)
            })
            || r.content.contains(token)
            || r.content.to_lowercase().contains(&token_lc)
    })
}
/// Semantic code search
pub(in crate::tools::dispatch) async fn search(
    service: &ContextFinderService,
    request: SearchRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let limit = request.limit.unwrap_or(10).clamp(1, 50);

    if request.query.trim().is_empty() {
        let meta = if response_mode == ResponseMode::Minimal {
            ToolMeta::default()
        } else {
            meta_for_request(service, request.path.as_deref()).await
        };
        return Ok(invalid_request_with_meta(
            "Error: Query cannot be empty",
            meta,
            None,
            Vec::new(),
        ));
    }

    let (root, root_display) = match service.resolve_root(request.path.as_deref()).await {
        Ok(value) => value,
        Err(message) => {
            let meta = if response_mode == ResponseMode::Minimal {
                ToolMeta::default()
            } else {
                meta_for_request(service, request.path.as_deref()).await
            };
            return Ok(invalid_request_with_meta(message, meta, None, Vec::new()));
        }
    };

    let policy = AutoIndexPolicy::from_request(request.auto_index, request.auto_index_budget_ms);
    let (mut engine, meta) = match service
        .prepare_semantic_engine_for_query(&root, policy, &request.query)
        .await
    {
        Ok(engine) => engine,
        Err(e) => {
            let message = format!("Error: {e}");
            let meta = service.tool_meta(&root).await;
            let meta_for_output = if response_mode == ResponseMode::Minimal {
                ToolMeta {
                    root_fingerprint: meta.root_fingerprint,
                    ..ToolMeta::default()
                }
            } else {
                meta
            };

            if is_semantic_unavailable_error(&message) {
                // Golden path: do not ask the agent to run `index`. The daemon will warm the index
                // in the background; meanwhile provide a fast lexical fallback.
                let budgets = super::super::mcp_default_budgets();
                let fallback_pattern = tokenize_focus_query(&request.query)
                    .into_iter()
                    .max_by_key(|t| t.len())
                    .unwrap_or_else(|| request.query.trim().to_string());
                let max_hunks = limit.min(8);
                let hunks = match grep_fallback_hunks(
                    &root,
                    &root_display,
                    &fallback_pattern,
                    response_mode,
                    max_hunks,
                    budgets.grep_context_max_chars,
                )
                .await
                {
                    Ok(hunks) => hunks,
                    Err(err) => {
                        return Ok(internal_error_with_meta(
                            format!("{message} (fallback grep failed: {err:#})"),
                            meta_for_output,
                        ));
                    }
                };

                let results: Vec<SearchResult> = hunks
                    .into_iter()
                    .take(limit)
                    .enumerate()
                    .map(|(idx, hunk)| SearchResult {
                        file: hunk.file,
                        start_line: hunk.start_line,
                        end_line: hunk.end_line,
                        symbol: None,
                        symbol_type: None,
                        // Scores are synthetic in fallback mode; keep stable ordering.
                        score: (1.0 - idx as f32 * 0.01).max(0.0),
                        content: hunk.content,
                    })
                    .collect();

                let mut next_actions = Vec::new();
                if response_mode == ResponseMode::Full {
                    next_actions.push(ToolNextAction {
                        tool: "doctor".to_string(),
                        args: serde_json::json!({ "path": root_display.clone() }),
                        reason: "If semantic search stays unavailable, doctor can explain why (CUDA/CPU, index drift, etc.).".to_string(),
                    });
                    if let Some(first) = results.first() {
                        next_actions.push(ToolNextAction {
                            tool: "read_pack".to_string(),
                            args: serde_json::json!({
                                "path": root_display.clone(),
                                "file": first.file.clone(),
                                "start_line": first.start_line,
                                "max_chars": budgets.read_pack_max_chars
                            }),
                            reason: "Open the top lexical hit with a bounded read_pack."
                                .to_string(),
                        });
                    }
                }

                let response = SearchResponse {
                    results,
                    next_actions,
                    meta: meta_for_output.clone(),
                };

                let mut doc = ContextDocBuilder::new();
                let answer = if response_mode == ResponseMode::Full {
                    format!("search: {} hits (fallback)", response.results.len())
                } else {
                    format!("search: {} hits", response.results.len())
                };
                doc.push_answer(&answer);
                doc.push_root_fingerprint(meta_for_output.root_fingerprint);
                if response_mode == ResponseMode::Full {
                    doc.push_note("diagnostic: semantic index unavailable; using lexical fallback");
                    doc.push_note(&format!("fallback_pattern: {fallback_pattern}"));
                }
                for (idx, hit) in response.results.iter().enumerate() {
                    if response_mode == ResponseMode::Full {
                        doc.push_note(&format!("hit {}: fallback score={:.3}", idx + 1, hit.score));
                    }
                    doc.push_ref_header(&hit.file, hit.start_line, hit.symbol.as_deref());
                    doc.push_block_smart(&hit.content);
                    doc.push_blank();
                }

                let output = CallToolResult::success(vec![Content::text(doc.finish())]);
                return Ok(attach_structured_content(
                    output,
                    &response,
                    meta_for_output,
                    "search",
                ));
            }

            return Ok(internal_error_with_meta(message, meta_for_output));
        }
    };
    let meta_for_output = if response_mode == ResponseMode::Minimal {
        ToolMeta {
            root_fingerprint: meta.root_fingerprint,
            ..ToolMeta::default()
        }
    } else {
        meta.clone()
    };

    let results = {
        match engine
            .engine_mut()
            .context_search
            .hybrid_mut()
            .search(&request.query, limit)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(internal_error_with_meta(
                    format!("Search error: {e}"),
                    meta_for_output,
                ));
            }
        }
    };
    let semantic_disabled_reason = engine
        .engine_mut()
        .context_search
        .hybrid()
        .semantic_disabled_reason()
        .map(str::to_string);

    let mut formatted: Vec<SearchResult> = results
        .into_iter()
        .map(|r| {
            let chunk = r.chunk;
            SearchResult {
                file: chunk.file_path,
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                symbol: chunk.metadata.symbol_name,
                symbol_type: chunk.metadata.chunk_type.map(|ct| ct.as_str().to_string()),
                score: r.score,
                content: chunk.content,
            }
        })
        .collect();

    let query_type = QueryClassifier::classify(&request.query);
    let docs_intent = QueryClassifier::is_docs_intent(&request.query);
    let query_tokens = tokenize_focus_query(&request.query);
    let anchor_token = choose_anchor_token(&query_tokens);
    let anchor_required =
        !docs_intent && matches!(query_type, QueryType::Identifier | QueryType::Path);

    // Even with a healthy semantic index, some repos (or queries) can yield zero semantic hits due
    // to aggressive profile thresholds, missing symbol metadata, or simply a stale/incorrect root.
    //
    // Agent-native UX: if semantic search returns no hits, fall back to a bounded lexical grep so
    // the agent still gets *some* anchored context and can debug root/index issues.
    let mut used_lexical_fallback = false;
    if anchor_required {
        if let Some(anchor) = anchor_token.as_deref() {
            if !formatted.is_empty() && !results_mention_token(&formatted, anchor) {
                // Guardrail: do not return high-confidence junk for anchored queries. Prefer
                // bounded filesystem strategies over unrelated semantic hits.
                formatted.clear();
            }
        }
    }
    if formatted.is_empty() {
        let budgets = super::super::mcp_default_budgets();
        let fallback_pattern = anchor_token
            .or_else(|| query_tokens.into_iter().max_by_key(|t| t.len()))
            .unwrap_or_else(|| request.query.trim().to_string());
        if !fallback_pattern.trim().is_empty() {
            let max_hunks = limit.min(8);
            if let Ok(hunks) = grep_fallback_hunks(
                &root,
                &root_display,
                &fallback_pattern,
                response_mode,
                max_hunks,
                budgets.grep_context_max_chars,
            )
            .await
            {
                if !hunks.is_empty() {
                    used_lexical_fallback = true;
                    formatted = hunks
                        .into_iter()
                        .take(limit)
                        .enumerate()
                        .map(|(idx, hunk)| SearchResult {
                            file: hunk.file,
                            start_line: hunk.start_line,
                            end_line: hunk.end_line,
                            symbol: None,
                            symbol_type: None,
                            // Synthetic, stable ordering in fallback mode.
                            score: (1.0 - idx as f32 * 0.01).max(0.0),
                            content: hunk.content,
                        })
                        .collect();
                }
            }
        }
    }

    let mut next_actions = Vec::new();
    if response_mode == ResponseMode::Full {
        let budgets = super::super::mcp_default_budgets();
        if formatted.is_empty() && semantic_disabled_reason.is_some() {
            let pattern = tokenize_focus_query(&request.query)
                .into_iter()
                .max_by_key(|t| t.len())
                .unwrap_or_else(|| request.query.trim().to_string());

            next_actions.push(ToolNextAction {
                tool: "grep_context".to_string(),
                args: serde_json::json!({
                    "path": root_display.clone(),
                    "pattern": pattern,
                    "literal": true,
                    "case_sensitive": false,
                    "context": 2,
                    "max_chars": budgets.grep_context_max_chars,
                    "max_hunks": 8,
                    "format": "numbered",
                    "response_mode": "facts"
                }),
                reason: "Semantic search is disabled; fall back to grep_context on the most relevant query token.".to_string(),
            });
        } else {
            next_actions.push(ToolNextAction {
                tool: "context_pack".to_string(),
                args: serde_json::json!({
                    "path": root_display.clone(),
                    "query": request.query.clone(),
                    "max_chars": budgets.context_pack_max_chars
                }),
                reason: "Build a bounded context pack for deeper context.".to_string(),
            });
            if let Some(first) = formatted.first() {
                next_actions.push(ToolNextAction {
                    tool: "read_pack".to_string(),
                    args: serde_json::json!({
                        "path": root_display.clone(),
                        "file": first.file.clone(),
                        "start_line": first.start_line,
                        "max_chars": budgets.read_pack_max_chars
                    }),
                    reason: "Open the top hit with a bounded read_pack.".to_string(),
                });
            }

            if anchor_required {
                if let Some(anchor) = choose_anchor_token(&tokenize_focus_query(&request.query)) {
                    if !formatted.is_empty() && !results_mention_token(&formatted, &anchor) {
                        next_actions.push(ToolNextAction {
                            tool: "text_search".to_string(),
                            args: serde_json::json!({
                                "path": root_display.clone(),
                                "pattern": anchor,
                                "max_results": 80,
                                "case_sensitive": false,
                                "whole_word": true,
                                "response_mode": "facts"
                            }),
                            reason: "Semantic hits do not mention the key anchor; verify the exact term via text_search (often reveals wrong root or typos).".to_string(),
                        });
                    }
                }
            }
        }
    }

    let response = SearchResponse {
        results: formatted,
        next_actions,
        meta: meta_for_output,
    };

    let mut doc = ContextDocBuilder::new();
    if used_lexical_fallback && response_mode == ResponseMode::Full {
        doc.push_answer(&format!(
            "search: {} hits (lexical fallback)",
            response.results.len()
        ));
    } else {
        doc.push_answer(&format!("search: {} hits", response.results.len()));
    }
    doc.push_root_fingerprint(response.meta.root_fingerprint);
    if response_mode == ResponseMode::Full {
        if let Some(reason) = semantic_disabled_reason.as_deref() {
            doc.push_note("semantic: disabled (embeddings unavailable; using fuzzy-only).");
            doc.push_note(&format!("semantic_error: {reason}"));
            if response.results.is_empty() {
                doc.push_note("next: grep_context (semantic disabled; fallback to literal grep)");
            }
        }
    }
    for (idx, hit) in response.results.iter().enumerate() {
        if response_mode == ResponseMode::Full {
            let mut meta_parts = Vec::new();
            meta_parts.push(format!("score={:.3}", hit.score));
            if let Some(kind) = hit.symbol_type.as_deref() {
                meta_parts.push(format!("type={kind}"));
            }
            doc.push_note(&format!("hit {}: {}", idx + 1, meta_parts.join(" ")));
        }
        doc.push_ref_header(&hit.file, hit.start_line, hit.symbol.as_deref());
        doc.push_block_smart(&hit.content);
        doc.push_blank();
    }
    let output = CallToolResult::success(vec![Content::text(doc.finish())]);
    Ok(attach_structured_content(
        output,
        &response,
        response.meta.clone(),
        "search",
    ))
}
