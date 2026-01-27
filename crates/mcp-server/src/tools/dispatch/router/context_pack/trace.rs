use super::super::super::{graph_language_key, ContextFinderService, QueryKind, QueryType};
use super::super::super::{Content, GraphLanguage};
use super::inputs::ContextPackInputs;

pub(super) fn append_trace_debug(
    contents: &mut Vec<Content>,
    service: &ContextFinderService,
    inputs: &ContextPackInputs,
    language: GraphLanguage,
    available_models: &[String],
) {
    let query_kind = match inputs.query_type {
        QueryType::Identifier => QueryKind::Identifier,
        QueryType::Path => QueryKind::Path,
        QueryType::Conceptual => QueryKind::Conceptual,
    };
    let desired_models: Vec<String> = service
        .profile
        .experts()
        .semantic_models(query_kind)
        .to_vec();
    let graph_nodes_cfg = service.profile.graph_nodes();

    let debug = serde_json::json!({
        "query_kind": format!("{query_kind:?}"),
        "strategy": format!("{:?}", inputs.strategy),
        "language": graph_language_key(language),
        "prefer_code": inputs.flags.prefer_code(),
        "include_docs": inputs.flags.include_docs(),
        "related_mode": inputs.related_mode.as_str(),
        "semantic_models": {
            "available": available_models,
            "desired": desired_models,
        },
        "graph_nodes": {
            "enabled": graph_nodes_cfg.enabled,
            "weight": graph_nodes_cfg.weight,
            "top_k": graph_nodes_cfg.top_k,
            "max_neighbors_per_relation": graph_nodes_cfg.max_neighbors_per_relation,
        }
    });
    contents.push(Content::text(
        context_protocol::serialize_json(&debug).unwrap_or_default(),
    ));
}
