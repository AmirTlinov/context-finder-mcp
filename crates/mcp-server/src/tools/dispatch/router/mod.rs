// Per-tool dispatch functions used by the MCP tool router.

pub(super) mod atlas_pack;
pub(super) mod batch;
pub(super) mod capabilities;
pub(super) mod context;
pub(super) mod context_pack;
pub(super) mod cursor_alias;
pub(super) mod doctor;
pub(super) mod error;
pub(super) mod evidence_fetch;
pub(super) mod explain;
pub(super) mod file_slice;
pub(super) mod grep_context;
pub(super) mod help;
pub(super) mod impact;
pub(super) mod list_files;
pub(super) mod ls;
pub(super) mod map;
pub(super) mod meaning_focus;
pub(super) mod meaning_pack;
pub(super) mod notebook_apply_suggest;
pub(super) mod notebook_edit;
pub(super) mod notebook_pack;
pub(super) mod notebook_suggest;
pub(super) mod overview;
pub(super) mod read_pack;
pub(super) mod repo_onboarding_pack;
pub(super) mod root;
pub(super) mod runbook_pack;
pub(super) mod search;
pub(super) mod semantic_fallback;
pub(super) mod text_search;
pub(super) mod trace;
pub(super) mod worktree_pack;

#[cfg(test)]
mod path_disambiguation_tests;
mod tool_router;

pub(super) fn build_tool_router_with_param_hints(
) -> super::tool_router_hints::ToolRouterWithParamHints<super::ContextFinderService> {
    tool_router::build_tool_router_with_param_hints()
}
