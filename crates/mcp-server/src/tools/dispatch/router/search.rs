use super::super::{
    tokenize_focus_query, AutoIndexPolicy, CallToolResult, Content, ContextFinderService, McpError,
    QueryClassifier, QueryType, ResponseMode, SearchRequest, SearchResponse, SearchResult,
    ToolMeta,
};
use crate::tools::chunk_summary::{push_hit_meta, trim_documentation, HitMeta};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::doc_context::{collect_doc_context, push_doc_context};

use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_request_with_meta,
    meta_for_request,
};
use super::semantic_fallback::{grep_fallback_hunks, is_semantic_unavailable_error};
use context_protocol::ToolNextAction;
use std::path::Path;

use crate::tools::dispatch::root::scope_hint_from_relative_path;

fn has_explicit_path_filters(request: &SearchRequest) -> bool {
    request
        .include_paths
        .as_ref()
        .is_some_and(|paths| paths.iter().any(|p| !p.trim().is_empty()))
        || request
            .exclude_paths
            .as_ref()
            .is_some_and(|paths| paths.iter().any(|p| !p.trim().is_empty()))
        || request
            .file_pattern
            .as_ref()
            .is_some_and(|pattern| !pattern.trim().is_empty())
}

fn normalize_filter_list(raw: Option<&Vec<String>>) -> Vec<String> {
    let Some(values) = raw else {
        return Vec::new();
    };
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .collect()
}

fn normalize_optional_pattern(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
}

pub(super) fn disambiguate_search_path_as_scope_hint_if_root_set(
    session_root: Option<&Path>,
    request: &mut SearchRequest,
) -> bool {
    let Some(session_root) = session_root else {
        return false;
    };
    let Some(raw_path) = request.path.as_deref() else {
        return false;
    };
    let raw_path = raw_path.trim();
    if raw_path.is_empty() {
        return false;
    }
    if has_explicit_path_filters(request) {
        return false;
    }

    let Some(hint) = scope_hint_from_relative_path(session_root, raw_path) else {
        return false;
    };
    if !hint.include_paths.is_empty() {
        request.include_paths = Some(hint.include_paths);
    }
    if hint.file_pattern.is_some() {
        request.file_pattern = hint.file_pattern;
    }
    request.path = None;
    true
}

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
    mut request: SearchRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let limit = request.limit.unwrap_or(10).clamp(1, 50);
    let session_root = { service.session.lock().await.clone_root().map(|(r, _)| r) };
    let _ =
        disambiguate_search_path_as_scope_hint_if_root_set(session_root.as_deref(), &mut request);
    let include_paths = normalize_filter_list(request.include_paths.as_ref());
    let exclude_paths = normalize_filter_list(request.exclude_paths.as_ref());
    let file_pattern = normalize_optional_pattern(request.file_pattern.as_deref());
    let path_filters_active = context_protocol::path_filters::is_active(
        &include_paths,
        &exclude_paths,
        file_pattern.as_deref(),
    );

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
                    .filter(|hunk| {
                        context_protocol::path_filters::path_allowed(
                            &hunk.file,
                            &include_paths,
                            &exclude_paths,
                            file_pattern.as_deref(),
                        )
                    })
                    .take(limit)
                    .enumerate()
                    .map(|(idx, hunk)| SearchResult {
                        file: hunk.file,
                        start_line: hunk.start_line,
                        end_line: hunk.end_line,
                        symbol: None,
                        symbol_type: None,
                        qualified_name: None,
                        parent_scope: None,
                        // Scores are synthetic in fallback mode; keep stable ordering.
                        score: (1.0 - idx as f32 * 0.01).max(0.0),
                        content: hunk.content,
                        documentation: None,
                        context_imports: Vec::new(),
                        tags: Vec::new(),
                        bundle_tags: Vec::new(),
                        related_paths: Vec::new(),
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
                if response_mode != ResponseMode::Minimal {
                    doc.push_root_fingerprint(meta_for_output.root_fingerprint);
                }
                if response_mode == ResponseMode::Full {
                    doc.push_note("diagnostic: semantic index unavailable; using lexical fallback");
                    doc.push_note(&format!("fallback_pattern: {fallback_pattern}"));
                }
                for (idx, hit) in response.results.iter().enumerate() {
                    if response_mode == ResponseMode::Full {
                        doc.push_note(&format!("hit {}: fallback score={:.3}", idx + 1, hit.score));
                    }
                    doc.push_ref_header(&hit.file, hit.start_line, hit.symbol.as_deref());
                    push_hit_meta(
                        &mut doc,
                        HitMeta {
                            documentation: hit.documentation.as_deref(),
                            chunk_type: hit.symbol_type.as_deref(),
                            qualified_name: hit.qualified_name.as_deref(),
                            parent_scope: hit.parent_scope.as_deref(),
                            tags: &hit.tags,
                            bundle_tags: &hit.bundle_tags,
                            context_imports: &hit.context_imports,
                            related_paths: &hit.related_paths,
                        },
                    );
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
            let metadata = chunk.metadata;
            let documentation = trim_documentation(metadata.documentation.as_deref());
            SearchResult {
                file: chunk.file_path,
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                symbol: metadata.symbol_name,
                symbol_type: metadata.chunk_type.map(|ct| ct.as_str().to_string()),
                qualified_name: metadata.qualified_name.clone(),
                parent_scope: metadata.parent_scope.clone(),
                score: r.score,
                content: chunk.content,
                documentation,
                context_imports: metadata.context_imports.clone(),
                tags: metadata.tags.clone(),
                bundle_tags: metadata.bundle_tags.clone(),
                related_paths: metadata.related_paths.clone(),
            }
        })
        .collect();
    if path_filters_active {
        formatted.retain(|hit| {
            context_protocol::path_filters::path_allowed(
                &hit.file,
                &include_paths,
                &exclude_paths,
                file_pattern.as_deref(),
            )
        });
    }

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
            .or_else(|| query_tokens.iter().max_by_key(|t| t.len()).cloned())
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
                        .filter(|hunk| {
                            context_protocol::path_filters::path_allowed(
                                &hunk.file,
                                &include_paths,
                                &exclude_paths,
                                file_pattern.as_deref(),
                            )
                        })
                        .take(limit)
                        .enumerate()
                        .map(|(idx, hunk)| SearchResult {
                            file: hunk.file,
                            start_line: hunk.start_line,
                            end_line: hunk.end_line,
                            symbol: None,
                            symbol_type: None,
                            qualified_name: None,
                            parent_scope: None,
                            // Synthetic, stable ordering in fallback mode.
                            score: (1.0 - idx as f32 * 0.01).max(0.0),
                            content: hunk.content,
                            documentation: None,
                            context_imports: Vec::new(),
                            tags: Vec::new(),
                            bundle_tags: Vec::new(),
                            related_paths: Vec::new(),
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
                tool: "rg".to_string(),
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
                reason:
                    "Semantic search is disabled; fall back to rg on the most relevant query token."
                        .to_string(),
            });
        } else {
            let mut args = serde_json::Map::new();
            args.insert(
                "path".to_string(),
                serde_json::Value::String(root_display.clone()),
            );
            args.insert(
                "query".to_string(),
                serde_json::Value::String(request.query.clone()),
            );
            args.insert(
                "max_chars".to_string(),
                serde_json::Value::Number(budgets.context_pack_max_chars.into()),
            );
            if !include_paths.is_empty() {
                args.insert(
                    "include_paths".to_string(),
                    serde_json::Value::Array(
                        include_paths
                            .iter()
                            .cloned()
                            .map(serde_json::Value::String)
                            .collect(),
                    ),
                );
            }
            if !exclude_paths.is_empty() {
                args.insert(
                    "exclude_paths".to_string(),
                    serde_json::Value::Array(
                        exclude_paths
                            .iter()
                            .cloned()
                            .map(serde_json::Value::String)
                            .collect(),
                    ),
                );
            }
            if let Some(pattern) = file_pattern.as_deref() {
                args.insert(
                    "file_pattern".to_string(),
                    serde_json::Value::String(pattern.to_string()),
                );
            }
            next_actions.push(ToolNextAction {
                tool: "context_pack".to_string(),
                args: serde_json::Value::Object(args),
                reason: "Build a bounded context pack for deeper context.".to_string(),
            });
            if let Some(first) = formatted.first() {
                let mut args = serde_json::Map::new();
                args.insert(
                    "path".to_string(),
                    serde_json::Value::String(root_display.clone()),
                );
                args.insert(
                    "file".to_string(),
                    serde_json::Value::String(first.file.clone()),
                );
                args.insert(
                    "start_line".to_string(),
                    serde_json::Value::Number(first.start_line.into()),
                );
                args.insert(
                    "max_chars".to_string(),
                    serde_json::Value::Number(budgets.read_pack_max_chars.into()),
                );
                if !include_paths.is_empty() {
                    args.insert(
                        "include_paths".to_string(),
                        serde_json::Value::Array(
                            include_paths
                                .iter()
                                .cloned()
                                .map(serde_json::Value::String)
                                .collect(),
                        ),
                    );
                }
                if !exclude_paths.is_empty() {
                    args.insert(
                        "exclude_paths".to_string(),
                        serde_json::Value::Array(
                            exclude_paths
                                .iter()
                                .cloned()
                                .map(serde_json::Value::String)
                                .collect(),
                        ),
                    );
                }
                if let Some(pattern) = file_pattern.as_deref() {
                    args.insert(
                        "file_pattern".to_string(),
                        serde_json::Value::String(pattern.to_string()),
                    );
                }
                next_actions.push(ToolNextAction {
                    tool: "read_pack".to_string(),
                    args: serde_json::Value::Object(args),
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
    if response_mode != ResponseMode::Minimal {
        doc.push_root_fingerprint(response.meta.root_fingerprint);
    }
    if response_mode == ResponseMode::Full {
        if let Some(reason) = semantic_disabled_reason.as_deref() {
            doc.push_note("semantic: disabled (embeddings unavailable; using fuzzy-only).");
            doc.push_note(&format!("semantic_error: {reason}"));
            if response.results.is_empty() {
                doc.push_note("next: rg (semantic disabled; fallback to regex search)");
            }
        }
    }
    if response_mode != ResponseMode::Minimal {
        let doc_snippets = collect_doc_context(
            &root,
            &query_tokens,
            &include_paths,
            &exclude_paths,
            file_pattern.as_deref(),
        );
        if !doc_snippets.is_empty() {
            push_doc_context(&mut doc, &doc_snippets);
        }
    }
    for (idx, hit) in response.results.iter().enumerate() {
        if response_mode == ResponseMode::Full {
            doc.push_note(&format!("hit {}: score={:.3}", idx + 1, hit.score));
        }
        doc.push_ref_header(&hit.file, hit.start_line, hit.symbol.as_deref());
        push_hit_meta(
            &mut doc,
            HitMeta {
                documentation: hit.documentation.as_deref(),
                chunk_type: hit.symbol_type.as_deref(),
                qualified_name: hit.qualified_name.as_deref(),
                parent_scope: hit.parent_scope.as_deref(),
                tags: &hit.tags,
                bundle_tags: &hit.bundle_tags,
                context_imports: &hit.context_imports,
                related_paths: &hit.related_paths,
            },
        );
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
