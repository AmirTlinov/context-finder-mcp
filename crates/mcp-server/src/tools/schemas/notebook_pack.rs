use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;
use crate::tools::notebook_types::{AgentRunbook, NotebookAnchor, NotebookScope};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NotebookPackRequest {
    /// Project directory path.
    #[schemars(description = "Project directory path (defaults to session root).")]
    pub path: Option<String>,

    /// Storage scope: project-local cache or user-repo cache.
    #[schemars(description = "Notebook scope: 'project' (default) or 'user_repo'.")]
    pub scope: Option<NotebookScope>,

    /// Maximum UTF-8 characters for the entire output (default: 2000).
    #[schemars(description = "Maximum number of UTF-8 characters for the notebook pack output.")]
    pub max_chars: Option<usize>,

    /// Response mode.
    #[schemars(description = "Response mode: 'facts' (default), 'full', or 'minimal'.")]
    pub response_mode: Option<ResponseMode>,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct NotebookPackBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct NotebookPackResult {
    pub version: u32,
    pub repo_id: String,
    pub anchors: Vec<NotebookAnchor>,
    pub runbooks: Vec<AgentRunbook>,
    pub budget: NotebookPackBudget,
    #[serde(default)]
    pub next_actions: Vec<context_protocol::ToolNextAction>,
    #[serde(default)]
    pub meta: ToolMeta,
}
