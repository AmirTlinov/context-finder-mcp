//! Context MCP tool surface.
//!
//! This module is intentionally split into submodules to keep schemas, dispatch, and per-tool
//! implementations reviewable and evolvable.

mod atlas_pack;
mod batch;
pub(crate) mod catalog;
mod context_doc;
mod context_legend;
mod cpv1;
mod cursor;
mod dispatch;
mod evidence_fetch;
mod external_memory;
mod file_slice;
mod grep_context;
mod list_files;
mod map;
mod meaning_diagram;
mod meaning_focus;
mod meaning_pack;
mod notebook_apply_suggest;
mod notebook_edit;
mod notebook_pack;
mod notebook_store;
mod notebook_suggest;
mod notebook_types;
mod paths;
mod repo_onboarding_pack;
mod runbook_pack;
mod schemas;
mod secrets;
mod util;
mod worktree_pack;

pub use dispatch::ContextFinderService;
