use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;
use crate::tools::notebook_types::NotebookScope;

#[derive(Debug, Deserialize, schemars::JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum RunbookPackMode {
    Summary,
    Section,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunbookPackRequest {
    /// Project directory path.
    #[schemars(description = "Project directory path (defaults to session root).")]
    pub path: Option<String>,

    /// Storage scope: project-local cache or user-repo cache.
    #[schemars(description = "Notebook scope: 'project' (default) or 'user_repo'.")]
    pub scope: Option<NotebookScope>,

    pub runbook_id: String,

    /// Output mode.
    #[schemars(description = "Mode: 'summary' (default) or 'section'.")]
    pub mode: Option<RunbookPackMode>,

    /// When mode=section, which section to expand.
    #[schemars(description = "Section id to expand when mode='section'.")]
    pub section_id: Option<String>,

    /// Continuation cursor for truncated section output.
    pub cursor: Option<String>,

    /// Maximum UTF-8 characters for the entire output (default: 2000).
    #[schemars(description = "Maximum number of UTF-8 characters for the runbook pack output.")]
    pub max_chars: Option<usize>,

    /// Response mode.
    #[schemars(description = "Response mode: 'facts' (default), 'full', or 'minimal'.")]
    pub response_mode: Option<ResponseMode>,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct RunbookPackBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct RunbookPackTocItem {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub status: String,
    #[serde(default)]
    pub total_items: u32,
    #[serde(default)]
    pub stale_items: u32,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct RunbookPackExpanded {
    pub section_id: String,
    pub content: String,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct RunbookPackResult {
    pub version: u32,
    pub runbook_id: String,
    #[serde(default)]
    pub runbook_title: String,
    pub mode: String,
    #[serde(default)]
    pub toc: Vec<RunbookPackTocItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expanded: Option<RunbookPackExpanded>,
    pub budget: RunbookPackBudget,
    #[serde(default)]
    pub next_actions: Vec<context_protocol::ToolNextAction>,
    #[serde(default)]
    pub meta: ToolMeta,
}
