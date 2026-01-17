use context_indexer::ToolMeta;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::tools::notebook_types::NotebookScope;

use super::notebook_suggest::NotebookSuggestResult;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotebookApplySuggestMode {
    Preview,
    Apply,
    Rollback,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotebookApplySuggestOverwritePolicy {
    /// Default: avoid overwriting anchors that appear manually edited.
    Safe,
    /// Overwrite existing anchors even if they appear manually edited.
    Force,
}

#[derive(Debug, Deserialize, schemars::JsonSchema, Clone)]
pub struct NotebookApplySuggestBackupPolicy {
    /// Create a backup snapshot before applying edits (default: true).
    pub create_backup: Option<bool>,
    /// Best-effort retention cap (default: 10). 0 means "do not delete old backups".
    pub max_backups: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum NotebookApplySuggestRequest {
    Preview {
        version: u32,

        /// Project directory path.
        #[schemars(description = "Project directory path (defaults to session root).")]
        path: Option<String>,

        /// Storage scope: project-local cache or user-repo cache.
        #[schemars(description = "Notebook scope: 'project' (default) or 'user_repo'.")]
        scope: Option<NotebookScope>,

        /// A NotebookSuggestResult object to preview/apply.
        suggestion: NotebookSuggestResult,

        /// If false, applying a truncated suggestion fails closed.
        #[schemars(description = "If false, applying a truncated suggestion fails closed.")]
        allow_truncated: Option<bool>,

        /// Overwrite policy (default: safe).
        #[schemars(description = "Overwrite policy: safe (default) or force.")]
        overwrite_policy: Option<NotebookApplySuggestOverwritePolicy>,

        /// Backup policy (ignored for preview).
        backup_policy: Option<NotebookApplySuggestBackupPolicy>,
    },
    Apply {
        version: u32,

        /// Project directory path.
        #[schemars(description = "Project directory path (defaults to session root).")]
        path: Option<String>,

        /// Storage scope: project-local cache or user-repo cache.
        #[schemars(description = "Notebook scope: 'project' (default) or 'user_repo'.")]
        scope: Option<NotebookScope>,

        /// A NotebookSuggestResult object to apply.
        suggestion: NotebookSuggestResult,

        /// If false, applying a truncated suggestion fails closed.
        #[schemars(description = "If false, applying a truncated suggestion fails closed.")]
        allow_truncated: Option<bool>,

        /// Overwrite policy (default: safe).
        #[schemars(description = "Overwrite policy: safe (default) or force.")]
        overwrite_policy: Option<NotebookApplySuggestOverwritePolicy>,

        /// Backup policy.
        backup_policy: Option<NotebookApplySuggestBackupPolicy>,
    },
    Rollback {
        version: u32,

        /// Project directory path.
        #[schemars(description = "Project directory path (defaults to session root).")]
        path: Option<String>,

        /// Storage scope: project-local cache or user-repo cache.
        #[schemars(description = "Notebook scope: 'project' (default) or 'user_repo'.")]
        scope: Option<NotebookScope>,

        /// Backup id returned by a prior apply.
        backup_id: String,
    },
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
#[serde(rename_all = "snake_case")]
pub enum NotebookApplySuggestChangeKind {
    Anchor,
    Runbook,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
#[serde(rename_all = "snake_case")]
pub enum NotebookApplySuggestChangeAction {
    Create,
    Update,
    Skip,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
#[serde(rename_all = "snake_case")]
pub enum NotebookApplySuggestSkipReason {
    NotManaged,
    ManualModified,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct NotebookApplySuggestChange {
    pub kind: NotebookApplySuggestChangeKind,
    pub id: String,
    pub action: NotebookApplySuggestChangeAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<NotebookApplySuggestSkipReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct NotebookApplySuggestSummary {
    pub anchors_before: usize,
    pub anchors_after: usize,
    pub runbooks_before: usize,
    pub runbooks_after: usize,
    pub new_anchors: usize,
    pub updated_anchors: usize,
    pub new_runbooks: usize,
    pub updated_runbooks: usize,
    #[serde(default)]
    pub skipped_anchors: usize,
    #[serde(default)]
    pub skipped_runbooks: usize,
    #[serde(default)]
    pub skipped_anchor_ids: Vec<String>,
    #[serde(default)]
    pub skipped_runbook_ids: Vec<String>,
    #[serde(default)]
    pub changes: Vec<NotebookApplySuggestChange>,
    #[serde(default)]
    pub touched_anchor_ids: Vec<String>,
    #[serde(default)]
    pub touched_runbook_ids: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema, Clone)]
pub struct NotebookApplySuggestResult {
    pub version: u32,
    pub mode: NotebookApplySuggestMode,
    pub repo_id: String,
    pub scope: NotebookScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_id: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub summary: NotebookApplySuggestSummary,
    #[serde(default)]
    pub next_actions: Vec<context_protocol::ToolNextAction>,
    #[serde(default)]
    pub meta: ToolMeta,
}
