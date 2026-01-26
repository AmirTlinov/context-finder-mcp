pub(super) use super::router::error::invalid_cursor_with_meta_details;
use super::router::error::tool_error;
use super::{
    encode_cursor, finalize_read_pack_budget, CallToolResult, Content, ContextFinderService,
    McpError, ReadPackBudget, ReadPackIntent, ReadPackNextAction, ReadPackRequest, ReadPackResult,
    ReadPackSection, ReadPackTruncation, ResponseMode, CURSOR_VERSION,
};
pub(super) use super::{
    ProjectFactsResult, ReadPackRecallResult, ReadPackSnippet, ReadPackSnippetKind,
};

mod context;
pub(crate) use context::ReadPackContext;

mod anchor_scan;
mod budget_trim;
mod candidates;
mod cursor_repair;
mod cursors;
mod file_cursor;
mod file_limits;
mod fs_scan;
mod grep_cursor;
mod intent_file;
mod intent_grep;
mod intent_memory;
mod intent_onboarding;
mod intent_query;
mod intent_recall;
mod intent_resolve;
mod memory_cursor;
mod memory_overview;
mod memory_snippets;
mod onboarding_command;
mod onboarding_docs;
mod onboarding_topics;
mod overlap;
mod prepare;
mod project_facts;
mod recall;
mod recall_cursor;
mod recall_directives;
mod recall_keywords;
mod recall_ops;
mod recall_paths;
mod recall_scoring;
mod recall_snippets;
mod recall_structural;
mod recall_trim;
mod render;
mod retry;
mod runner;
mod session;

pub(super) use super::decode_cursor;
pub(super) use cursors::trimmed_non_empty_str;
pub(in crate::tools::dispatch) use runner::read_pack;

use render::entrypoint_candidate_score;

const VERSION: u32 = 1;
const DEFAULT_MAX_CHARS: usize = 6_000;
const MIN_MAX_CHARS: usize = 400;
const MAX_MAX_CHARS: usize = 500_000;
const DEFAULT_GREP_CONTEXT: usize = 20;
const MAX_GREP_MATCHES: usize = 10_000;
const MAX_GREP_HUNKS: usize = 200;
// Agent-native default: keep tool calls snappy so the agent can stay in a tight loop.
// Callers can always opt in to longer work via `timeout_ms` (and/or `deep` for recall).
const DEFAULT_TIMEOUT_MS: u64 = 12_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const MAX_RECALL_INLINE_CURSOR_CHARS: usize = 1_200;

type ToolResult<T> = std::result::Result<T, CallToolResult>;

fn call_error(code: &'static str, message: impl Into<String>) -> CallToolResult {
    tool_error(code, message)
}

const REASON_ANCHOR_FOCUS_FILE: &str = "anchor:focus_file";
const REASON_ANCHOR_DOC: &str = "anchor:doc";
const REASON_ANCHOR_ENTRYPOINT: &str = "anchor:entrypoint";
const REASON_NEEDLE_GREP_HUNK: &str = "needle:grep_hunk";
const REASON_NEEDLE_FILE_SLICE: &str = "needle:cat";
const REASON_HALO_CONTEXT_PACK_PRIMARY: &str = "halo:context_pack_primary";
const REASON_HALO_CONTEXT_PACK_RELATED: &str = "halo:context_pack_related";
const REASON_INTENT_FILE: &str = "intent:file";

#[cfg(test)]
mod tests;
