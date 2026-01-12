//! Context MCP tool surface.
//!
//! This module is intentionally split into submodules to keep schemas, dispatch, and per-tool
//! implementations reviewable and evolvable.

mod batch;
pub(crate) mod catalog;
mod context_doc;
mod context_legend;
mod cursor;
mod dispatch;
mod evidence_fetch;
mod external_memory;
mod file_slice;
mod grep_context;
mod list_files;
mod map;
mod meaning_common;
mod meaning_diagram;
mod meaning_focus;
mod meaning_pack;
mod paths;
mod repo_onboarding_pack;
mod schemas;
mod secrets;
mod util;

pub use dispatch::ContextFinderService;
