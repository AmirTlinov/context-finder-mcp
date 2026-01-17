use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Storage scope for notebooks/runbooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotebookScope {
    Project,
    UserRepo,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NotebookRepo {
    pub repo_id: String,
    #[serde(default)]
    pub repo_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotebookAnchorKind {
    Canon,
    Ci,
    Contract,
    Entrypoint,
    Zone,
    Work,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotebookLocatorKind {
    Symbol,
    PathGlob,
    Grep,
    SnippetSha256,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NotebookLocator {
    pub kind: NotebookLocatorKind,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NotebookEvidencePointer {
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NotebookAnchor {
    pub id: String,
    pub kind: NotebookAnchorKind,
    pub label: String,
    pub evidence: Vec<NotebookEvidencePointer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<NotebookLocator>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunbookDefaultMode {
    #[default]
    Summary,
    Section,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunbookPolicy {
    #[serde(default)]
    pub default_mode: RunbookDefaultMode,
    #[serde(default = "default_noise_budget")]
    pub noise_budget: f32,
    #[serde(default = "default_max_items_per_section")]
    pub max_items_per_section: u32,
}

fn default_noise_budget() -> f32 {
    0.2
}

fn default_max_items_per_section() -> u32 {
    10
}

impl Default for RunbookPolicy {
    fn default() -> Self {
        Self {
            default_mode: RunbookDefaultMode::Summary,
            noise_budget: default_noise_budget(),
            max_items_per_section: default_max_items_per_section(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunbookSection {
    Anchors {
        id: String,
        title: String,
        anchor_ids: Vec<String>,
        #[serde(default = "default_true")]
        include_evidence: bool,
    },
    MeaningPack {
        id: String,
        title: String,
        query: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_chars: Option<u32>,
    },
    Worktrees {
        id: String,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_chars: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentRunbook {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub policy: RunbookPolicy,
    pub sections: Vec<RunbookSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentNotebook {
    pub version: u32,
    pub repo: NotebookRepo,
    #[serde(default)]
    pub anchors: Vec<NotebookAnchor>,
    #[serde(default)]
    pub runbooks: Vec<AgentRunbook>,
}
