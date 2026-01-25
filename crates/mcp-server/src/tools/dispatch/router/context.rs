use super::super::{
    tokenize_focus_query, AutoIndexPolicy, CallToolResult, Content, ContextFinderService,
    ContextHit, ContextRequest, ContextResult, McpError, RelatedCode, ResponseMode, ToolMeta,
};
use crate::tools::chunk_summary::{push_hit_meta, trim_documentation, HitMeta};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::doc_context::{collect_doc_context, push_doc_context};

use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_request_with_meta,
    meta_for_request,
};
use super::semantic_fallback::{grep_fallback_hunks, is_semantic_unavailable_error};
use std::path::Path;

use crate::tools::dispatch::root::scope_hint_from_relative_path;

fn has_explicit_path_filters(request: &ContextRequest) -> bool {
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

pub(super) fn disambiguate_context_path_as_scope_hint_if_root_set(
    session_root: Option<&Path>,
    request: &mut ContextRequest,
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
/// Search with graph context
pub(in crate::tools::dispatch) async fn context(
    service: &ContextFinderService,
    mut request: ContextRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let limit = request.limit.unwrap_or(5).clamp(1, 20);
    let strategy = match request.strategy.as_deref() {
        Some("direct") => context_graph::AssemblyStrategy::Direct,
        Some("deep") => context_graph::AssemblyStrategy::Deep,
        _ => context_graph::AssemblyStrategy::Extended,
    };

    let session_root = { service.session.lock().await.clone_root().map(|(r, _)| r) };
    let _ =
        disambiguate_context_path_as_scope_hint_if_root_set(session_root.as_deref(), &mut request);
    let include_paths = normalize_filter_list(request.include_paths.as_ref());
    let exclude_paths = normalize_filter_list(request.exclude_paths.as_ref());
    let file_pattern = normalize_optional_pattern(request.file_pattern.as_deref());
    let path_filters_active = context_protocol::path_filters::is_active(
        &include_paths,
        &exclude_paths,
        file_pattern.as_deref(),
    );
    let query_tokens = tokenize_focus_query(&request.query);

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

    let (root, root_display) = match service
        .resolve_root_for_tool(request.path.as_deref(), "context")
        .await
    {
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
                let budgets = super::super::mcp_default_budgets();
                let fallback_pattern = super::super::tokenize_focus_query(&request.query)
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

                let results: Vec<ContextHit> = hunks
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
                    .map(|(idx, hunk)| ContextHit {
                        file: hunk.file,
                        start_line: hunk.start_line,
                        end_line: hunk.end_line,
                        symbol: None,
                        chunk_type: None,
                        qualified_name: None,
                        parent_scope: None,
                        score: (1.0 - idx as f32 * 0.01).max(0.0),
                        content: hunk.content,
                        documentation: None,
                        context_imports: Vec::new(),
                        tags: Vec::new(),
                        bundle_tags: Vec::new(),
                        related_paths: Vec::new(),
                        related: Vec::new(),
                    })
                    .collect();

                let result = ContextResult {
                    results,
                    related_count: 0,
                    meta: meta_for_output.clone(),
                };

                let mut doc = ContextDocBuilder::new();
                let answer = if response_mode == ResponseMode::Full {
                    format!("context: {} hits (fallback)", result.results.len())
                } else {
                    format!("context: {} hits", result.results.len())
                };
                doc.push_answer(&answer);
                if response_mode != ResponseMode::Minimal {
                    doc.push_root_fingerprint(meta_for_output.root_fingerprint);
                }
                if response_mode == ResponseMode::Full {
                    doc.push_note("diagnostic: semantic index unavailable; using lexical fallback");
                    doc.push_note(&format!("fallback_pattern: {fallback_pattern}"));
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
                for hit in &result.results {
                    doc.push_ref_header(&hit.file, hit.start_line, hit.symbol.as_deref());
                    push_hit_meta(
                        &mut doc,
                        HitMeta {
                            documentation: hit.documentation.as_deref(),
                            chunk_type: hit.chunk_type.as_deref(),
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
                    &result,
                    meta_for_output,
                    "context",
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

    let enriched = {
        let language = request.language.as_deref().map_or_else(
            || {
                ContextFinderService::detect_language(
                    engine.engine_mut().context_search.hybrid().chunks(),
                )
            },
            |lang| ContextFinderService::parse_language(Some(lang)),
        );

        if let Err(e) = engine.engine_mut().ensure_graph(language).await {
            return Ok(internal_error_with_meta(
                format!("Graph build error: {e}"),
                meta_for_output.clone(),
            ));
        }

        match engine
            .engine_mut()
            .context_search
            .search_with_context(&request.query, limit, strategy)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(internal_error_with_meta(
                    format!("Search error: {e}"),
                    meta_for_output.clone(),
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

    drop(engine);

    let mut related_count = 0;
    let results: Vec<ContextHit> = enriched
        .into_iter()
        .filter_map(|er| {
            if path_filters_active
                && !context_protocol::path_filters::path_allowed(
                    &er.primary.chunk.file_path,
                    &include_paths,
                    &exclude_paths,
                    file_pattern.as_deref(),
                )
            {
                return None;
            }

            let related: Vec<RelatedCode> = er
                .related
                .iter()
                .filter(|rc| {
                    !path_filters_active
                        || context_protocol::path_filters::path_allowed(
                            &rc.chunk.file_path,
                            &include_paths,
                            &exclude_paths,
                            file_pattern.as_deref(),
                        )
                })
                .take(5)
                .map(|rc| {
                    related_count += 1;
                    RelatedCode {
                        file: rc.chunk.file_path.clone(),
                        lines: format!("{}-{}", rc.chunk.start_line, rc.chunk.end_line),
                        symbol: rc.chunk.metadata.symbol_name.clone(),
                        relationship: rc.relationship_path.join(" -> "),
                    }
                })
                .collect();

            let metadata = er.primary.chunk.metadata;
            let documentation = trim_documentation(metadata.documentation.as_deref());
            let chunk_type = metadata.chunk_type.map(|ct| ct.as_str().to_string());
            let qualified_name = metadata.qualified_name.clone();
            let parent_scope = metadata.parent_scope.clone();
            let tags = metadata.tags.clone();
            let bundle_tags = metadata.bundle_tags.clone();
            let context_imports = metadata.context_imports.clone();
            let related_paths = metadata.related_paths.clone();
            let symbol = metadata.symbol_name;
            Some(ContextHit {
                file: er.primary.chunk.file_path,
                start_line: er.primary.chunk.start_line,
                end_line: er.primary.chunk.end_line,
                symbol,
                chunk_type,
                qualified_name,
                parent_scope,
                score: er.primary.score,
                content: er.primary.chunk.content,
                documentation,
                context_imports,
                tags,
                bundle_tags,
                related_paths,
                related,
            })
        })
        .collect();

    let result = ContextResult {
        results,
        related_count,
        meta: meta_for_output,
    };

    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!(
        "context: {} hits (related={})",
        result.results.len(),
        result.related_count
    ));
    if response_mode != ResponseMode::Minimal {
        doc.push_root_fingerprint(result.meta.root_fingerprint);
    }
    if response_mode == ResponseMode::Full {
        if let Some(reason) = semantic_disabled_reason.as_deref() {
            doc.push_note("semantic: disabled (embeddings unavailable; using fuzzy-only).");
            doc.push_note(&format!("semantic_error: {reason}"));
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
    for hit in &result.results {
        doc.push_ref_header(&hit.file, hit.start_line, hit.symbol.as_deref());
        push_hit_meta(
            &mut doc,
            HitMeta {
                documentation: hit.documentation.as_deref(),
                chunk_type: hit.chunk_type.as_deref(),
                qualified_name: hit.qualified_name.as_deref(),
                parent_scope: hit.parent_scope.as_deref(),
                tags: &hit.tags,
                bundle_tags: &hit.bundle_tags,
                context_imports: &hit.context_imports,
                related_paths: &hit.related_paths,
            },
        );
        doc.push_block_smart(&hit.content);
        for related in &hit.related {
            let sym = related.symbol.as_deref().unwrap_or("unknown");
            doc.push_line(&format!(
                "N: related {}:{} {sym} ({})",
                related.file, related.lines, related.relationship
            ));
        }
        doc.push_blank();
    }
    let output = CallToolResult::success(vec![Content::text(doc.finish())]);
    Ok(attach_structured_content(
        output,
        &result,
        result.meta.clone(),
        "context",
    ))
}
