use super::fallback;
use super::graph_nodes;
use super::helpers;
use super::inputs;
use super::response;

use super::super::super::{
    current_model_id, pack_enriched_results, prepare_context_pack_enriched, AutoIndexPolicy,
    CallToolResult, ContextFinderService, ContextPackOutput, ContextPackRequest, McpError,
    CONTEXT_PACK_VERSION,
};
use super::super::error::{
    attach_meta, internal_error_with_meta, invalid_request_with_root_context, meta_for_request,
};
use super::super::semantic_fallback::is_semantic_unavailable_error;

use context_indexer::{AnchorPolicy, RetrievalMode, ToolTrustMeta};
use context_search::{count_anchor_hits, detect_primary_anchor};

pub(in crate::tools::dispatch) async fn context_pack(
    service: &ContextFinderService,
    mut request: ContextPackRequest,
) -> Result<CallToolResult, McpError> {
    let session_root = { service.session.lock().await.clone_root().map(|(r, _)| r) };
    let _ = helpers::disambiguate_context_pack_path_as_scope_hint_if_root_set(
        session_root.as_deref(),
        &mut request,
    );

    let requested_anchor_policy = request.anchor_policy.unwrap_or_default();
    let anchor_policy = helpers::effective_anchor_policy(requested_anchor_policy);
    let primary_anchor = detect_primary_anchor(&request.query);

    let inputs = match inputs::parse_inputs(&request) {
        Ok(parsed) => parsed,
        Err(err) => {
            let meta = meta_for_request(service, request.path.as_deref()).await;
            return Ok(attach_meta(err, meta));
        }
    };

    let mut hints: Vec<String> = Vec::new();
    if !inputs.include_paths.is_empty() {
        hints.extend(inputs.include_paths.iter().cloned());
    }
    if !inputs.exclude_paths.is_empty() {
        hints.extend(inputs.exclude_paths.iter().cloned());
    }
    if let Some(pattern) = inputs.file_pattern.as_deref() {
        if !pattern.trim().is_empty() {
            hints.push(pattern.to_string());
        }
    }
    let (root, root_display) = match service
        .resolve_root_with_hints_for_tool(inputs.path.as_deref(), &hints, "context_pack")
        .await
    {
        Ok(value) => value,
        Err(message) => {
            let meta = meta_for_request(service, inputs.path.as_deref()).await;
            return Ok(
                invalid_request_with_root_context(service, message, meta, None, Vec::new()).await,
            );
        }
    };

    let policy = AutoIndexPolicy::from_request(request.auto_index, request.auto_index_budget_ms);
    let (mut engine, meta) = match service
        .prepare_semantic_engine_for_query(&root, policy, &request.query)
        .await
    {
        Ok(engine) => engine,
        Err(err) => {
            let message = format!("Error: {err}");
            let meta = service.tool_meta(&root).await;
            if is_semantic_unavailable_error(&message) {
                let fallback_pattern = primary_anchor
                    .as_ref()
                    .map(|anchor| anchor.normalized.clone())
                    .or_else(|| inputs.query_tokens.iter().max_by_key(|t| t.len()).cloned())
                    .unwrap_or_else(|| request.query.trim().to_string());

                let mut fallback_meta = meta.clone();
                fallback_meta.trust = Some(match primary_anchor.as_ref() {
                    Some(anchor) => ToolTrustMeta {
                        retrieval_mode: Some(RetrievalMode::Lexical),
                        fallback_used: Some(true),
                        anchor_policy: Some(anchor_policy),
                        anchor_detected: Some(true),
                        anchor_kind: Some(anchor.kind),
                        anchor_primary: Some(anchor.normalized.clone()),
                        anchor_hits: None,
                        anchor_not_found: None,
                    },
                    None => ToolTrustMeta {
                        retrieval_mode: Some(RetrievalMode::Lexical),
                        fallback_used: Some(true),
                        anchor_policy: Some(anchor_policy),
                        anchor_detected: Some(false),
                        anchor_kind: None,
                        anchor_primary: None,
                        anchor_hits: None,
                        anchor_not_found: None,
                    },
                });

                match fallback::build_lexical_fallback_result(
                    service,
                    &root,
                    &root_display,
                    &inputs,
                    fallback::LexicalFallbackArgs {
                        query: &request.query,
                        fallback_pattern: &fallback_pattern,
                        meta: fallback_meta,
                        reason_note: Some(
                            "diagnostic: semantic index unavailable; using lexical fallback",
                        ),
                    },
                )
                .await
                {
                    Ok(result) => return Ok(result),
                    Err(err) => return Ok(attach_meta(err, service.tool_meta(&root).await)),
                }
            }

            return Ok(internal_error_with_meta(message, meta));
        }
    };

    let language = helpers::select_language(request.language.as_deref(), &mut engine);
    if let Err(err) = engine.engine_mut().ensure_graph(language).await {
        return Ok(internal_error_with_meta(
            format!("Graph build error: {err}"),
            meta.clone(),
        ));
    }

    let available_models = engine.engine_mut().loaded_model_ids();
    let source_index_mtime_ms =
        super::super::super::unix_ms(engine.engine_mut().canonical_index_mtime);

    let mut enriched = match engine
        .engine_mut()
        .context_search
        .search_with_context(&request.query, inputs.candidate_limit, inputs.strategy)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return Ok(internal_error_with_meta(
                format!("Search error: {e}"),
                meta.clone(),
            ));
        }
    };
    let semantic_disabled_reason = engine
        .engine_mut()
        .context_search
        .hybrid()
        .semantic_disabled_reason()
        .map(str::to_string);

    let graph_nodes_ctx = graph_nodes::GraphNodesContext {
        root: &root,
        language,
        query: &request.query,
        query_type: inputs.query_type,
        strategy: inputs.strategy,
        candidate_limit: inputs.candidate_limit,
        source_index_mtime_ms,
    };
    if let Err(err) =
        graph_nodes::maybe_apply_graph_nodes(service, graph_nodes_ctx, &mut enriched, &mut engine)
            .await
    {
        return Ok(attach_meta(err, meta.clone()));
    }

    drop(engine);

    let enriched = prepare_context_pack_enriched(
        enriched,
        inputs.limit,
        inputs.flags.prefer_code(),
        inputs.flags.include_docs(),
    );
    let (items, budget) = pack_enriched_results(
        &service.profile,
        enriched,
        inputs.max_chars,
        inputs.max_related_per_primary,
        &inputs.include_paths,
        &inputs.exclude_paths,
        inputs.file_pattern.as_deref(),
        inputs.related_mode,
        &inputs.query_tokens,
    );

    let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    let query = request.query.clone();
    let mut output = ContextPackOutput {
        version: CONTEXT_PACK_VERSION,
        query: query.clone(),
        model_id,
        profile: service.profile.name().to_string(),
        items,
        budget,
        next_actions: Vec::new(),
        meta,
    };

    let retrieval_mode = if semantic_disabled_reason.is_some() {
        RetrievalMode::Lexical
    } else {
        RetrievalMode::Hybrid
    };
    output.meta.trust = Some(match primary_anchor.as_ref() {
        Some(anchor) => {
            let hits = count_anchor_hits(&output.items, anchor);
            ToolTrustMeta {
                retrieval_mode: Some(retrieval_mode),
                fallback_used: Some(false),
                anchor_policy: Some(anchor_policy),
                anchor_detected: Some(true),
                anchor_kind: Some(anchor.kind),
                anchor_primary: Some(anchor.normalized.clone()),
                anchor_hits: Some(hits),
                anchor_not_found: Some(hits == 0 && output.items.is_empty()),
            }
        }
        None => ToolTrustMeta {
            retrieval_mode: Some(retrieval_mode),
            fallback_used: Some(false),
            anchor_policy: Some(anchor_policy),
            anchor_detected: Some(false),
            anchor_kind: None,
            anchor_primary: None,
            anchor_hits: None,
            anchor_not_found: None,
        },
    });

    // Output gate: enforce strong-anchor coverage (fail-closed) as the last step before render.
    if anchor_policy != AnchorPolicy::Off {
        if let Some(anchor) = primary_anchor.as_ref() {
            let hits = count_anchor_hits(&output.items, anchor);
            if hits == 0 {
                let mut fallback_meta = output.meta.clone();
                fallback_meta.trust = Some(ToolTrustMeta {
                    retrieval_mode: Some(RetrievalMode::Lexical),
                    fallback_used: Some(true),
                    anchor_policy: Some(anchor_policy),
                    anchor_detected: Some(true),
                    anchor_kind: Some(anchor.kind),
                    anchor_primary: Some(anchor.normalized.clone()),
                    anchor_hits: None,
                    anchor_not_found: None,
                });

                match fallback::build_lexical_fallback_result(
                    service,
                    &root,
                    &root_display,
                    &inputs,
                    fallback::LexicalFallbackArgs {
                        query: &request.query,
                        fallback_pattern: &anchor.normalized,
                        meta: fallback_meta,
                        reason_note: Some("semantic: anchor_missing (fallback to filesystem)"),
                    },
                )
                .await
                {
                    Ok(result) => return Ok(result),
                    Err(err) => {
                        return Ok(attach_meta(
                            err,
                            meta_for_request(service, inputs.path.as_deref()).await,
                        ))
                    }
                }
            }
        }
    }

    response::finalize_context_pack(response::FinalizeContextPackArgs {
        service,
        inputs: &inputs,
        root_display: &root_display,
        query: &query,
        output,
        semantic_disabled_reason,
        language,
        available_models: &available_models,
    })
    .await
}
