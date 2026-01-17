use anyhow::Result;
use context_indexer::ToolMeta;
use std::path::Path;

use super::notebook_store::{
    load_or_init_notebook, notebook_paths_for_scope, resolve_repo_identity,
};
use super::notebook_types::NotebookScope;
use super::schemas::notebook_pack::{NotebookPackBudget, NotebookPackRequest, NotebookPackResult};

const VERSION: u32 = 1;
const DEFAULT_MAX_CHARS: usize = 2_000;
const MIN_MAX_CHARS: usize = 800;
const MAX_MAX_CHARS: usize = 500_000;

pub(super) async fn compute_notebook_pack_result(
    root: &Path,
    request: &NotebookPackRequest,
) -> Result<NotebookPackResult> {
    let scope = request.scope.unwrap_or(NotebookScope::Project);
    let max_chars = request
        .max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);

    let identity = resolve_repo_identity(root);
    let paths = notebook_paths_for_scope(root, scope, &identity)?;
    let notebook = load_or_init_notebook(root, &paths)?;

    Ok(NotebookPackResult {
        version: VERSION,
        repo_id: identity.repo_id,
        anchors: notebook.anchors,
        runbooks: notebook.runbooks,
        budget: NotebookPackBudget {
            max_chars,
            used_chars: 0,
            truncated: false,
        },
        next_actions: Vec::new(),
        meta: ToolMeta::default(),
    })
}
