use context_indexer::ToolMeta;
use context_protocol::BudgetTruncation;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::content_format::ContentFormat;
use super::response_mode::ResponseMode;
use super::ToolNextAction;
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FileSliceRequest {
    /// Project directory path
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_FINDER_ROOT/CONTEXT_FINDER_PROJECT_ROOT, git root, then cwd)."
    )]
    pub path: Option<String>,

    /// File path (relative to project root). Optional when continuing via `cursor`.
    #[schemars(
        description = "File path (relative to project root). Optional when continuing via cursor"
    )]
    pub file: Option<String>,

    /// First line to include (1-based, default: 1). Alias: offset
    #[schemars(description = "First line to include (1-based). Alias: offset")]
    #[serde(alias = "offset")]
    pub start_line: Option<usize>,

    /// Maximum number of lines to return (default: 200). Alias: limit
    #[schemars(description = "Maximum number of lines to return (bounded). Alias: limit")]
    #[serde(alias = "limit")]
    pub max_lines: Option<usize>,

    /// Hard `max_chars` budget for the `.context` response (including envelope).
    ///
    /// The tool will truncate content as needed to stay within the budget and (when applicable)
    /// return a cursor so the agent can continue pagination. Under extremely small budgets the
    /// returned slice may be tiny or empty, but the tool avoids failing solely due to `max_chars`.
    #[schemars(
        description = "Hard max_chars budget for the .context response (including envelope)."
    )]
    pub max_chars: Option<usize>,

    /// Render format for the returned content (default: plain).
    #[schemars(
        description = "Render format for returned content: 'plain' (default) or 'numbered'"
    )]
    pub format: Option<ContentFormat>,

    /// Response mode:
    /// - "minimal" (default): lowest noise; strips most diagnostics and next_actions, but keeps provenance meta (`root_fingerprint`).
    /// - "facts": payload-focused; keeps lightweight counters/budget info and provenance meta (`root_fingerprint`), but strips next_actions.
    /// - "full": includes meta/diagnostics (freshness index_state) and next_actions (when applicable).
    #[schemars(description = "Response mode: 'minimal' (default), 'facts', or 'full'")]
    pub response_mode: Option<ResponseMode>,

    /// Allow reading potential secret files (default: false).
    ///
    /// When false, the tool refuses to read common secret locations (e.g. `.env`, SSH keys,
    /// `*.pem`/`*.key`) to prevent accidental leakage in agent context windows.
    #[schemars(description = "Allow reading potential secret files (default: false).")]
    pub allow_secrets: Option<bool>,

    /// Opaque cursor token to continue a previous response. When provided, `start_line` is ignored.
    #[schemars(description = "Opaque cursor token to continue a previous file_slice response")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::tools) struct FileSliceCursorV1 {
    pub(in crate::tools) v: u32,
    pub(in crate::tools) tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools) root_hash: Option<u64>,
    pub(in crate::tools) file: String,
    pub(in crate::tools) max_lines: usize,
    pub(in crate::tools) max_chars: usize,
    #[serde(default)]
    pub(in crate::tools) format: ContentFormat,
    #[serde(default)]
    pub(in crate::tools) allow_secrets: bool,
    pub(in crate::tools) next_start_line: usize,
    pub(in crate::tools) next_byte_offset: u64,
    pub(in crate::tools) file_size_bytes: u64,
    pub(in crate::tools) file_mtime_ms: u64,
}

pub type FileSliceTruncation = BudgetTruncation;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FileSliceResult {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returned_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<usize>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<FileSliceTruncation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_actions: Option<Vec<ToolNextAction>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_mtime_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    pub content: String,
}
