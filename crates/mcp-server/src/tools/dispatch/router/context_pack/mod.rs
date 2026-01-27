mod budget;
mod fallback;
mod graph_nodes;
mod handler;
mod inputs;
mod render;
mod response;
mod trace;

#[cfg(test)]
mod tests;

type ToolResult<T> = std::result::Result<T, rmcp::model::CallToolResult>;

pub(in crate::tools::dispatch) use handler::context_pack;

#[cfg(test)]
pub(super) fn disambiguate_context_pack_path_as_scope_hint_if_root_set(
    session_root: Option<&std::path::Path>,
    request: &mut super::super::ContextPackRequest,
) -> bool {
    handler::disambiguate_context_pack_path_as_scope_hint_if_root_set(session_root, request)
}
