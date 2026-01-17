use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;
use crate::tools::notebook_types::{AgentRunbook, NotebookAnchor, NotebookScope};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NotebookSuggestRequest {
    /// Project directory path.
    #[schemars(description = "Project directory path (defaults to session root).")]
    pub path: Option<String>,

    /// Storage scope hint for generated notebook edits (default: project).
    #[schemars(description = "Notebook scope hint: 'project' (default) or 'user_repo'.")]
    pub scope: Option<NotebookScope>,

    /// Optional query to steer suggestions (defaults to an onboarding-style query).
    #[schemars(description = "Optional query to steer notebook suggestions.")]
    pub query: Option<String>,

    /// Maximum UTF-8 characters for the entire output (default: 2000).
    #[schemars(
        description = "Maximum number of UTF-8 characters for the notebook suggest output."
    )]
    pub max_chars: Option<usize>,

    /// Response mode.
    #[schemars(description = "Response mode: 'facts' (default), 'full', or 'minimal'.")]
    pub response_mode: Option<ResponseMode>,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct NotebookSuggestBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct NotebookSuggestResult {
    pub version: u32,
    pub repo_id: String,
    pub query: String,
    pub anchors: Vec<NotebookAnchor>,
    pub runbooks: Vec<AgentRunbook>,
    pub budget: NotebookSuggestBudget,
    #[serde(default)]
    pub next_actions: Vec<context_protocol::ToolNextAction>,
    #[serde(default)]
    pub meta: ToolMeta,
}
