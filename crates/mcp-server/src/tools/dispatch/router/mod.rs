// Per-tool dispatch functions used by the MCP tool router.

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
pub(super) mod map;
pub(super) mod meaning_focus;
pub(super) mod meaning_pack;
pub(super) mod overview;
pub(super) mod read_pack;
pub(super) mod repo_onboarding_pack;
pub(super) mod search;
pub(super) mod semantic_fallback;
pub(super) mod text_search;
pub(super) mod trace;
