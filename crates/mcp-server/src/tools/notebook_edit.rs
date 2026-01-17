use anyhow::{anyhow, Context as AnyhowContext, Result};
use std::collections::HashSet;
use std::path::Path;
use std::time::SystemTime;

use super::notebook_store::{
    acquire_notebook_lock, fill_missing_source_hashes, load_or_init_notebook,
    notebook_paths_for_scope, resolve_repo_identity, save_notebook,
};
use super::notebook_types::{NotebookAnchor, NotebookScope, RunbookSection};
use super::schemas::notebook_edit::{NotebookEditOp, NotebookEditRequest};
use super::secrets::is_potential_secret_path;
use super::util::unix_ms;

#[derive(Debug, Default)]
pub(crate) struct NotebookEditSummary {
    pub applied_ops: usize,
    pub anchors: usize,
    pub runbooks: usize,
    pub touched_anchor_ids: Vec<String>,
    pub touched_runbook_ids: Vec<String>,
}

pub(super) async fn apply_notebook_edit(
    root: &Path,
    request: &NotebookEditRequest,
) -> Result<NotebookEditSummary> {
    if request.version != 1 {
        anyhow::bail!("Unsupported notebook_edit version {}", request.version);
    }
    let scope = request.scope.unwrap_or(NotebookScope::Project);
    let identity = resolve_repo_identity(root);
    let paths = notebook_paths_for_scope(root, scope, &identity)?;

    let _lock = acquire_notebook_lock(&paths.lock_path)?;
    let mut notebook = load_or_init_notebook(root, &paths)?;

    let now = unix_ms(SystemTime::now()).to_string();
    notebook.repo.updated_at = Some(now.clone());
    if notebook.repo.created_at.is_none() {
        notebook.repo.created_at = Some(now.clone());
    }

    let mut touched_anchor_ids: HashSet<String> = HashSet::new();
    let mut touched_runbook_ids: HashSet<String> = HashSet::new();

    for op in &request.ops {
        match op {
            NotebookEditOp::UpsertAnchor { anchor } => {
                let mut anchor = anchor.clone();
                validate_anchor(root, &anchor)?;
                if anchor.created_at.as_deref().unwrap_or("").is_empty() {
                    anchor.created_at = Some(now.clone());
                }
                anchor.updated_at = Some(now.clone());
                fill_missing_source_hashes(root, &mut anchor)?;

                let id = anchor.id.clone();
                upsert_anchor(&mut notebook.anchors, anchor)?;
                touched_anchor_ids.insert(id);
            }
            NotebookEditOp::DeleteAnchor { id } => {
                ensure_anchor_not_referenced(&notebook.runbooks, id)?;
                let before = notebook.anchors.len();
                notebook.anchors.retain(|a| a.id != *id);
                if notebook.anchors.len() == before {
                    return Err(anyhow!("Anchor not found: {id}"));
                }
                touched_anchor_ids.insert(id.clone());
            }
            NotebookEditOp::UpsertRunbook { runbook } => {
                let mut runbook = runbook.clone();
                validate_runbook(&notebook.anchors, &runbook)?;
                if runbook.created_at.as_deref().unwrap_or("").is_empty() {
                    runbook.created_at = Some(now.clone());
                }
                runbook.updated_at = Some(now.clone());

                let id = runbook.id.clone();
                upsert_runbook(&mut notebook.runbooks, runbook)?;
                touched_runbook_ids.insert(id);
            }
            NotebookEditOp::DeleteRunbook { id } => {
                let before = notebook.runbooks.len();
                notebook.runbooks.retain(|rb| rb.id != *id);
                if notebook.runbooks.len() == before {
                    return Err(anyhow!("Runbook not found: {id}"));
                }
                touched_runbook_ids.insert(id.clone());
            }
        }
    }

    save_notebook(&paths, &notebook)?;

    let mut summary = NotebookEditSummary {
        applied_ops: request.ops.len(),
        anchors: notebook.anchors.len(),
        runbooks: notebook.runbooks.len(),
        touched_anchor_ids: touched_anchor_ids.into_iter().collect(),
        touched_runbook_ids: touched_runbook_ids.into_iter().collect(),
    };
    summary.touched_anchor_ids.sort();
    summary.touched_runbook_ids.sort();
    Ok(summary)
}

fn validate_anchor(root: &Path, anchor: &NotebookAnchor) -> Result<()> {
    let id = anchor.id.trim();
    if id.is_empty() {
        anyhow::bail!("Anchor id must not be empty");
    }
    if anchor.label.trim().is_empty() {
        anyhow::bail!("Anchor label must not be empty");
    }
    if anchor.evidence.is_empty() {
        anyhow::bail!("Anchor evidence must not be empty");
    }
    for ev in &anchor.evidence {
        let file = ev.file.trim();
        if file.is_empty() {
            anyhow::bail!("Anchor evidence file must not be empty");
        }
        if Path::new(file).is_absolute() {
            anyhow::bail!("Anchor evidence file must be repo-relative (got absolute path)");
        }
        let rel = file.replace('\\', "/");
        if is_potential_secret_path(&rel) {
            anyhow::bail!("Refusing to store potential secret path in notebook evidence: {rel}");
        }
        let canonical = root
            .join(Path::new(&rel))
            .canonicalize()
            .with_context(|| format!("Failed to resolve evidence path '{rel}'"))?;
        if !canonical.starts_with(root) {
            anyhow::bail!("Evidence file '{rel}' is outside project root");
        }
        if ev.start_line == 0 || ev.end_line == 0 {
            anyhow::bail!("Evidence start/end lines must be >= 1");
        }
        if ev.end_line < ev.start_line {
            anyhow::bail!("Evidence end_line must be >= start_line");
        }
    }
    Ok(())
}

fn upsert_anchor(anchors: &mut Vec<NotebookAnchor>, anchor: NotebookAnchor) -> Result<()> {
    if let Some(existing) = anchors.iter_mut().find(|a| a.id == anchor.id) {
        *existing = anchor;
        return Ok(());
    }
    anchors.push(anchor);
    Ok(())
}

fn validate_runbook(
    anchors: &[NotebookAnchor],
    runbook: &crate::tools::notebook_types::AgentRunbook,
) -> Result<()> {
    if runbook.id.trim().is_empty() {
        anyhow::bail!("Runbook id must not be empty");
    }
    if runbook.title.trim().is_empty() {
        anyhow::bail!("Runbook title must not be empty");
    }
    if runbook.sections.is_empty() {
        anyhow::bail!("Runbook must have at least one section");
    }
    let known: HashSet<&str> = anchors.iter().map(|a| a.id.as_str()).collect();
    for section in &runbook.sections {
        match section {
            RunbookSection::Anchors { anchor_ids, .. } => {
                if anchor_ids.is_empty() {
                    anyhow::bail!("Anchors section must have at least one anchor_id");
                }
                for id in anchor_ids {
                    if !known.contains(id.as_str()) {
                        anyhow::bail!("Runbook references unknown anchor id: {id}");
                    }
                }
            }
            RunbookSection::MeaningPack {
                query, max_chars, ..
            } => {
                if query.trim().is_empty() {
                    anyhow::bail!("MeaningPack section query must not be empty");
                }
                if let Some(max_chars) = max_chars {
                    if *max_chars < 800 {
                        anyhow::bail!("MeaningPack section max_chars must be >= 800");
                    }
                }
            }
            RunbookSection::Worktrees {
                max_chars, limit, ..
            } => {
                if let Some(max_chars) = max_chars {
                    if *max_chars < 800 {
                        anyhow::bail!("Worktrees section max_chars must be >= 800");
                    }
                }
                if let Some(limit) = limit {
                    if *limit == 0 || *limit > 200 {
                        anyhow::bail!("Worktrees section limit must be 1..=200");
                    }
                }
            }
        }
    }
    Ok(())
}

fn upsert_runbook(
    runbooks: &mut Vec<crate::tools::notebook_types::AgentRunbook>,
    runbook: crate::tools::notebook_types::AgentRunbook,
) -> Result<()> {
    if let Some(existing) = runbooks.iter_mut().find(|rb| rb.id == runbook.id) {
        *existing = runbook;
        return Ok(());
    }
    runbooks.push(runbook);
    Ok(())
}

fn ensure_anchor_not_referenced(
    runbooks: &[crate::tools::notebook_types::AgentRunbook],
    anchor_id: &str,
) -> Result<()> {
    let mut refs = Vec::new();
    for rb in runbooks {
        for section in &rb.sections {
            if let RunbookSection::Anchors { anchor_ids, .. } = section {
                if anchor_ids.iter().any(|id| id == anchor_id) {
                    refs.push(rb.id.clone());
                }
            }
        }
    }
    if !refs.is_empty() {
        refs.sort();
        anyhow::bail!(
            "Cannot delete anchor '{anchor_id}': referenced by runbooks: {}",
            refs.join(", ")
        );
    }
    Ok(())
}
