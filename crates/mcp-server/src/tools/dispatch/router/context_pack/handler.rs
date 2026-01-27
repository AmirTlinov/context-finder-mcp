use super::fallback;
use super::graph_nodes;
use super::inputs;
use super::response;

use super::super::super::{
    current_model_id, pack_enriched_results, prepare_context_pack_enriched, AutoIndexPolicy,
    CallToolResult, ContextFinderService, ContextPackOutput, ContextPackRequest, McpError,
    QueryClassifier, QueryType, CONTEXT_PACK_VERSION,
};
use super::super::error::{
    attach_meta, internal_error_with_meta, invalid_request_with_root_context, meta_for_request,
};
use super::super::semantic_fallback::is_semantic_unavailable_error;

use std::path::Path;

fn select_language(
    raw: Option<&str>,
    engine: &mut super::super::super::EngineLock,
) -> context_graph::GraphLanguage {
    raw.map_or_else(
        || {
            let chunks = engine.engine_mut().context_search.hybrid().chunks();
            ContextFinderService::detect_language(chunks)
        },
        |lang| ContextFinderService::parse_language(Some(lang)),
    )
}

pub(super) fn disambiguate_context_pack_path_as_scope_hint_if_root_set(
    session_root: Option<&Path>,
    request: &mut ContextPackRequest,
) -> bool {
    let has_explicit_filters = request
        .include_paths
        .as_ref()
        .is_some_and(|v| !v.is_empty())
        || request
            .exclude_paths
            .as_ref()
            .is_some_and(|v| !v.is_empty())
        || request
            .file_pattern
            .as_deref()
            .map(str::trim)
            .is_some_and(|v| !v.is_empty());
    if has_explicit_filters {
        return false;
    }

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
    if Path::new(raw_path).is_absolute() {
        return false;
    }

    let normalized = raw_path.replace('\\', "/");
    if normalized.contains('*') || normalized.contains('?') {
        request.file_pattern = Some(normalized);
        request.path = None;
        return true;
    }

    let candidate = session_root.join(&normalized);
    let is_dir = std::fs::metadata(&candidate)
        .ok()
        .map(|meta| meta.is_dir())
        .unwrap_or(false);
    if is_dir {
        request.include_paths = Some(vec![normalized]);
    } else {
        request.file_pattern = Some(normalized);
    }
    request.path = None;
    true
}

pub(in crate::tools::dispatch) async fn context_pack(
    service: &ContextFinderService,
    mut request: ContextPackRequest,
) -> Result<CallToolResult, McpError> {
    let session_root = { service.session.lock().await.clone_root().map(|(r, _)| r) };
    let _ = disambiguate_context_pack_path_as_scope_hint_if_root_set(
        session_root.as_deref(),
        &mut request,
    );

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
                let fallback_pattern = inputs
                    .query_tokens
                    .iter()
                    .max_by_key(|t| t.len())
                    .cloned()
                    .unwrap_or_else(|| request.query.trim().to_string());

                match fallback::build_lexical_fallback_result(
                    service,
                    &root,
                    &root_display,
                    &inputs,
                    fallback::LexicalFallbackArgs {
                        query: &request.query,
                        fallback_pattern: &fallback_pattern,
                        meta,
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

    let language = select_language(request.language.as_deref(), &mut engine);
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
    let output = ContextPackOutput {
        version: CONTEXT_PACK_VERSION,
        query: query.clone(),
        model_id,
        profile: service.profile.name().to_string(),
        items,
        budget,
        next_actions: Vec::new(),
        meta,
    };

    // Guardrail: if a query contains a strong anchor (identifier/path), never return a pack that
    // doesn't mention it. This prevents "high-confidence junk" in mixed queries like
    // "LintWarning struct definition" when the identifier is missing from the repo.
    if !output.items.is_empty()
        && matches!(inputs.query_type, QueryType::Identifier | QueryType::Path)
        && !QueryClassifier::is_docs_intent(&request.query)
    {
        if let Some(anchor) = fallback::choose_fallback_token(&inputs.query_tokens) {
            if !fallback::items_mention_token(&output.items, &anchor) {
                match fallback::build_lexical_fallback_result(
                    service,
                    &root,
                    &root_display,
                    &inputs,
                    fallback::LexicalFallbackArgs {
                        query: &request.query,
                        fallback_pattern: &anchor,
                        meta: output.meta.clone(),
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
