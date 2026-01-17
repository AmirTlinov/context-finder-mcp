use rmcp::schemars;
use serde::Deserialize;

use crate::tools::notebook_types::{AgentRunbook, NotebookAnchor, NotebookScope};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum NotebookEditOp {
    UpsertAnchor { anchor: NotebookAnchor },
    DeleteAnchor { id: String },
    UpsertRunbook { runbook: AgentRunbook },
    DeleteRunbook { id: String },
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NotebookEditRequest {
    pub version: u32,

    /// Project directory path.
    #[schemars(description = "Project directory path (defaults to session root).")]
    pub path: Option<String>,

    /// Storage scope: project-local cache or user-repo cache.
    #[schemars(description = "Notebook scope: 'project' (default) or 'user_repo'.")]
    pub scope: Option<NotebookScope>,

    pub ops: Vec<NotebookEditOp>,
}
