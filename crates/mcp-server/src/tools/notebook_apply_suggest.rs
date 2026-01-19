use anyhow::{anyhow, Context as AnyhowContext, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::notebook_store::{
    acquire_notebook_lock, fill_missing_source_hashes, load_or_init_notebook,
    notebook_paths_for_scope, resolve_repo_identity, save_notebook,
};
use super::notebook_types::{
    AgentNotebook, AgentRunbook, NotebookAnchor, NotebookEvidencePointer, NotebookScope,
    RunbookPolicy, RunbookSection,
};
use super::schemas::notebook_apply_suggest::{
    NotebookApplySuggestBackupPolicy, NotebookApplySuggestChange, NotebookApplySuggestChangeAction,
    NotebookApplySuggestChangeKind, NotebookApplySuggestMode, NotebookApplySuggestOverwritePolicy,
    NotebookApplySuggestRequest, NotebookApplySuggestSkipReason, NotebookApplySuggestSummary,
};
use super::util::{hex_encode_lower, unix_ms};

const SUGGEST_FP_TAG_PREFIX: &str = "cf_suggest_fp=";

fn default_create_backup(policy: Option<&NotebookApplySuggestBackupPolicy>) -> bool {
    policy.and_then(|p| p.create_backup).unwrap_or(true)
}

fn default_max_backups(policy: Option<&NotebookApplySuggestBackupPolicy>) -> u32 {
    policy.and_then(|p| p.max_backups).unwrap_or(10)
}

#[derive(Debug)]
pub(crate) struct NotebookApplySuggestOutcome {
    pub mode: NotebookApplySuggestMode,
    pub repo_id: String,
    pub scope: NotebookScope,
    pub backup_id: Option<String>,
    pub warnings: Vec<String>,
    pub summary: NotebookApplySuggestSummary,
}

pub(super) async fn apply_notebook_apply_suggest(
    root: &Path,
    request: &NotebookApplySuggestRequest,
) -> Result<NotebookApplySuggestOutcome> {
    match request {
        NotebookApplySuggestRequest::Preview {
            version,
            scope,
            suggestion,
            allow_truncated,
            overwrite_policy,
            backup_policy,
            ..
        } => {
            ensure_version(*version)?;
            apply_or_preview(
                root,
                NotebookApplySuggestMode::Preview,
                scope.unwrap_or(NotebookScope::Project),
                suggestion,
                allow_truncated.unwrap_or(false),
                overwrite_policy.unwrap_or(NotebookApplySuggestOverwritePolicy::Safe),
                backup_policy.as_ref(),
            )
        }
        NotebookApplySuggestRequest::Apply {
            version,
            scope,
            suggestion,
            allow_truncated,
            overwrite_policy,
            backup_policy,
            ..
        } => {
            ensure_version(*version)?;
            apply_or_preview(
                root,
                NotebookApplySuggestMode::Apply,
                scope.unwrap_or(NotebookScope::Project),
                suggestion,
                allow_truncated.unwrap_or(false),
                overwrite_policy.unwrap_or(NotebookApplySuggestOverwritePolicy::Safe),
                backup_policy.as_ref(),
            )
        }
        NotebookApplySuggestRequest::Rollback {
            version,
            scope,
            backup_id,
            ..
        } => {
            ensure_version(*version)?;
            rollback(root, scope.unwrap_or(NotebookScope::Project), backup_id)
        }
    }
}

fn ensure_version(version: u32) -> Result<()> {
    if version != 1 {
        anyhow::bail!("Unsupported notebook_apply_suggest version {version}");
    }
    Ok(())
}

fn apply_or_preview(
    root: &Path,
    mode: NotebookApplySuggestMode,
    scope: NotebookScope,
    suggestion: &crate::tools::schemas::notebook_suggest::NotebookSuggestResult,
    allow_truncated: bool,
    overwrite_policy: NotebookApplySuggestOverwritePolicy,
    backup_policy: Option<&NotebookApplySuggestBackupPolicy>,
) -> Result<NotebookApplySuggestOutcome> {
    if suggestion.version != 1 {
        anyhow::bail!(
            "Unsupported notebook_suggest version {}",
            suggestion.version
        );
    }
    if suggestion.budget.truncated && mode == NotebookApplySuggestMode::Apply && !allow_truncated {
        anyhow::bail!(
            "Refusing to apply truncated suggestion (fail-closed). Re-run notebook_suggest with a higher max_chars or pass allow_truncated=true."
        );
    }

    let identity = resolve_repo_identity(root);
    if suggestion.repo_id != identity.repo_id {
        anyhow::bail!(
            "Suggestion repo_id mismatch (expected {}, got {})",
            identity.repo_id,
            suggestion.repo_id
        );
    }

    let paths = notebook_paths_for_scope(root, scope, &identity)?;
    let _lock = acquire_notebook_lock(&paths.lock_path)?;

    let notebook = load_or_init_notebook(root, &paths)?;
    let anchors_before = notebook.anchors.len();
    let runbooks_before = notebook.runbooks.len();

    let mut warnings: Vec<String> = Vec::new();
    if suggestion.budget.truncated {
        warnings.push("suggestion_truncated".to_string());
    }

    let now = unix_ms(SystemTime::now()).to_string();

    if mode == NotebookApplySuggestMode::Preview {
        let mut preview = notebook.clone();
        let counts =
            apply_suggestion_to_notebook(root, &mut preview, suggestion, overwrite_policy, &now)?;
        if counts.changes_truncated {
            warnings.push("changes_truncated".to_string());
        }
        let summary = NotebookApplySuggestSummary {
            anchors_before,
            anchors_after: preview.anchors.len(),
            runbooks_before,
            runbooks_after: preview.runbooks.len(),
            new_anchors: counts.new_anchors,
            updated_anchors: counts.updated_anchors,
            new_runbooks: counts.new_runbooks,
            updated_runbooks: counts.updated_runbooks,
            skipped_anchors: counts.skipped_anchors,
            skipped_runbooks: counts.skipped_runbooks,
            skipped_anchor_ids: counts.skipped_anchor_ids,
            skipped_runbook_ids: counts.skipped_runbook_ids,
            changes: counts.changes,
            touched_anchor_ids: counts.touched_anchor_ids,
            touched_runbook_ids: counts.touched_runbook_ids,
        };
        return Ok(NotebookApplySuggestOutcome {
            mode,
            repo_id: identity.repo_id,
            scope,
            backup_id: None,
            warnings,
            summary,
        });
    }

    // Apply mode: take a backup snapshot first, then write.
    let mut notebook = notebook;
    let backup_id = if default_create_backup(backup_policy) {
        let bytes = serde_json::to_vec_pretty(&notebook).context("serialize notebook backup")?;
        let backup_id = generate_backup_id(&bytes)?;
        let (backup_id, backup_path) = unique_backup_path(&paths.notebook_path, &backup_id)?;
        write_atomic(&backup_path, &bytes)?;
        cleanup_old_backups(&paths.notebook_path, default_max_backups(backup_policy))?;
        Some(backup_id)
    } else {
        None
    };

    // Mutate + save.
    notebook.repo.updated_at = Some(now.clone());
    if notebook.repo.created_at.is_none() {
        notebook.repo.created_at = Some(now.clone());
    }

    let counts =
        apply_suggestion_to_notebook(root, &mut notebook, suggestion, overwrite_policy, &now)?;
    if counts.changes_truncated {
        warnings.push("changes_truncated".to_string());
    }
    save_notebook(&paths, &notebook)?;

    let summary = NotebookApplySuggestSummary {
        anchors_before,
        anchors_after: notebook.anchors.len(),
        runbooks_before,
        runbooks_after: notebook.runbooks.len(),
        new_anchors: counts.new_anchors,
        updated_anchors: counts.updated_anchors,
        new_runbooks: counts.new_runbooks,
        updated_runbooks: counts.updated_runbooks,
        skipped_anchors: counts.skipped_anchors,
        skipped_runbooks: counts.skipped_runbooks,
        skipped_anchor_ids: counts.skipped_anchor_ids,
        skipped_runbook_ids: counts.skipped_runbook_ids,
        changes: counts.changes,
        touched_anchor_ids: counts.touched_anchor_ids,
        touched_runbook_ids: counts.touched_runbook_ids,
    };

    Ok(NotebookApplySuggestOutcome {
        mode,
        repo_id: identity.repo_id,
        scope,
        backup_id,
        warnings,
        summary,
    })
}

fn rollback(
    root: &Path,
    scope: NotebookScope,
    backup_id: &str,
) -> Result<NotebookApplySuggestOutcome> {
    let identity = resolve_repo_identity(root);
    let paths = notebook_paths_for_scope(root, scope, &identity)?;
    let _lock = acquire_notebook_lock(&paths.lock_path)?;

    let mut current = load_or_init_notebook(root, &paths)?;
    let anchors_before = current.anchors.len();
    let runbooks_before = current.runbooks.len();

    let backup_path = backup_path_for_id(&paths.notebook_path, backup_id);
    let raw = std::fs::read_to_string(&backup_path)
        .with_context(|| format!("read notebook backup {}", backup_path.display()))?;
    let mut restored: AgentNotebook = serde_json::from_str(&raw)
        .with_context(|| format!("parse notebook backup {}", backup_path.display()))?;

    if restored.version != 1 {
        anyhow::bail!("Unsupported notebook backup version {}", restored.version);
    }
    if restored.repo.repo_id != identity.repo_id {
        anyhow::bail!(
            "Backup repo_id mismatch (expected {}, got {})",
            identity.repo_id,
            restored.repo.repo_id
        );
    }

    let now = unix_ms(SystemTime::now()).to_string();
    restored.repo.updated_at = Some(now.clone());
    if restored.repo.created_at.is_none() {
        restored.repo.created_at = Some(now.clone());
    }

    // Compute a coarse 'touched' set for operator visibility (symmetric diff).
    let before_anchor_ids: HashSet<&str> = current.anchors.iter().map(|a| a.id.as_str()).collect();
    let after_anchor_ids: HashSet<&str> = restored.anchors.iter().map(|a| a.id.as_str()).collect();
    let mut touched_anchor_ids: Vec<String> = before_anchor_ids
        .symmetric_difference(&after_anchor_ids)
        .map(|v| (*v).to_string())
        .collect();
    touched_anchor_ids.sort();

    let before_runbook_ids: HashSet<&str> =
        current.runbooks.iter().map(|rb| rb.id.as_str()).collect();
    let after_runbook_ids: HashSet<&str> =
        restored.runbooks.iter().map(|rb| rb.id.as_str()).collect();
    let mut touched_runbook_ids: Vec<String> = before_runbook_ids
        .symmetric_difference(&after_runbook_ids)
        .map(|v| (*v).to_string())
        .collect();
    touched_runbook_ids.sort();

    // Save restored notebook.
    current = restored;
    save_notebook(&paths, &current)?;

    Ok(NotebookApplySuggestOutcome {
        mode: NotebookApplySuggestMode::Rollback,
        repo_id: identity.repo_id,
        scope,
        backup_id: Some(backup_id.to_string()),
        warnings: Vec::new(),
        summary: NotebookApplySuggestSummary {
            anchors_before,
            anchors_after: current.anchors.len(),
            runbooks_before,
            runbooks_after: current.runbooks.len(),
            new_anchors: 0,
            updated_anchors: 0,
            new_runbooks: 0,
            updated_runbooks: 0,
            skipped_anchors: 0,
            skipped_runbooks: 0,
            skipped_anchor_ids: Vec::new(),
            skipped_runbook_ids: Vec::new(),
            changes: Vec::new(),
            touched_anchor_ids,
            touched_runbook_ids,
        },
    })
}

#[derive(Debug, Default)]
struct ApplyCounts {
    new_anchors: usize,
    updated_anchors: usize,
    new_runbooks: usize,
    updated_runbooks: usize,
    skipped_anchors: usize,
    skipped_runbooks: usize,
    skipped_anchor_ids: Vec<String>,
    skipped_runbook_ids: Vec<String>,
    changes: Vec<NotebookApplySuggestChange>,
    changes_truncated: bool,
    touched_anchor_ids: Vec<String>,
    touched_runbook_ids: Vec<String>,
}

fn apply_suggestion_to_notebook(
    root: &Path,
    notebook: &mut AgentNotebook,
    suggestion: &crate::tools::schemas::notebook_suggest::NotebookSuggestResult,
    overwrite_policy: NotebookApplySuggestOverwritePolicy,
    now: &str,
) -> Result<ApplyCounts> {
    ensure_unique_ids(suggestion.anchors.iter().map(|a| a.id.as_str()), "anchors")?;
    ensure_unique_ids(
        suggestion.runbooks.iter().map(|rb| rb.id.as_str()),
        "runbooks",
    )?;

    let mut touched_anchor_ids: HashSet<String> = HashSet::new();
    let mut touched_runbook_ids: HashSet<String> = HashSet::new();
    let mut counts = ApplyCounts::default();

    // Anchors first: runbooks may reference them.
    for incoming in &suggestion.anchors {
        let mut anchor = incoming.clone();
        validate_anchor(root, &anchor)?;

        let existing_anchor = notebook.anchors.iter().find(|a| a.id == anchor.id);
        let existed = existing_anchor.is_some();

        if let Some(existing_anchor) = existing_anchor {
            if overwrite_policy == NotebookApplySuggestOverwritePolicy::Safe {
                if let Some(reason) = should_skip_anchor(existing_anchor)? {
                    counts.skipped_anchors += 1;
                    counts.skipped_anchor_ids.push(incoming.id.clone());
                    if counts.changes.len() < 50 {
                        let hint = skip_change_hint(&reason);
                        counts.changes.push(NotebookApplySuggestChange {
                            kind: NotebookApplySuggestChangeKind::Anchor,
                            id: incoming.id.clone(),
                            action: NotebookApplySuggestChangeAction::Skip,
                            reason: Some(reason),
                            hint,
                        });
                    } else {
                        counts.changes_truncated = true;
                    }
                    continue;
                }
            }
            preserve_existing_source_hashes(existing_anchor, &mut anchor);
        }

        if anchor.created_at.as_deref().unwrap_or("").is_empty() {
            anchor.created_at = Some(now.to_string());
        }
        anchor.updated_at = Some(now.to_string());
        fill_missing_source_hashes(root, &mut anchor)?;
        set_suggest_fp_tag(&mut anchor)?;

        if counts.changes.len() < 50 {
            counts.changes.push(NotebookApplySuggestChange {
                kind: NotebookApplySuggestChangeKind::Anchor,
                id: incoming.id.clone(),
                action: if existed {
                    NotebookApplySuggestChangeAction::Update
                } else {
                    NotebookApplySuggestChangeAction::Create
                },
                reason: None,
                hint: if existed {
                    anchor_change_hint(existing_anchor, &anchor)
                } else {
                    anchor_create_hint(&anchor)
                },
            });
        } else {
            counts.changes_truncated = true;
        }

        upsert_anchor(&mut notebook.anchors, anchor)?;
        touched_anchor_ids.insert(incoming.id.clone());
        if existed {
            counts.updated_anchors += 1;
        } else {
            counts.new_anchors += 1;
        }
    }

    // Runbooks
    for incoming in &suggestion.runbooks {
        let mut runbook = incoming.clone();
        validate_runbook(&notebook.anchors, &runbook)?;

        let existing_runbook = notebook.runbooks.iter().find(|rb| rb.id == runbook.id);
        let existed = existing_runbook.is_some();
        let mut adopt_hint: Option<String> = None;
        if let Some(existing) = existing_runbook {
            if overwrite_policy == NotebookApplySuggestOverwritePolicy::Safe {
                if let Some(reason) = should_skip_runbook(existing)? {
                    match reason {
                        NotebookApplySuggestSkipReason::NotManaged => {
                            // Safe adoption: if the existing runbook matches the incoming
                            // suggestion, treat it as managed by attaching a fingerprint tag.
                            let existing_fp = compute_runbook_fingerprint(existing)?;
                            let incoming_fp = compute_runbook_fingerprint(&runbook)?;
                            if existing_fp == incoming_fp {
                                adopt_hint = Some("adopted: now managed".to_string());
                            } else {
                                counts.skipped_runbooks += 1;
                                counts.skipped_runbook_ids.push(incoming.id.clone());
                                if counts.changes.len() < 50 {
                                    let hint = skip_change_hint(&reason);
                                    counts.changes.push(NotebookApplySuggestChange {
                                        kind: NotebookApplySuggestChangeKind::Runbook,
                                        id: incoming.id.clone(),
                                        action: NotebookApplySuggestChangeAction::Skip,
                                        reason: Some(reason),
                                        hint,
                                    });
                                } else {
                                    counts.changes_truncated = true;
                                }
                                continue;
                            }
                        }
                        NotebookApplySuggestSkipReason::ManualModified => {
                            counts.skipped_runbooks += 1;
                            counts.skipped_runbook_ids.push(incoming.id.clone());
                            if counts.changes.len() < 50 {
                                let hint = skip_change_hint(&reason);
                                counts.changes.push(NotebookApplySuggestChange {
                                    kind: NotebookApplySuggestChangeKind::Runbook,
                                    id: incoming.id.clone(),
                                    action: NotebookApplySuggestChangeAction::Skip,
                                    reason: Some(reason),
                                    hint,
                                });
                            } else {
                                counts.changes_truncated = true;
                            }
                            continue;
                        }
                    }
                }
            }
        }

        let mut change_hint = if existed {
            runbook_change_hint(existing_runbook, &runbook)
        } else {
            runbook_create_hint(&runbook)
        };
        if change_hint.is_none() {
            change_hint = adopt_hint;
        }

        if runbook.created_at.as_deref().unwrap_or("").is_empty() {
            runbook.created_at = Some(now.to_string());
        }
        runbook.updated_at = Some(now.to_string());
        set_runbook_suggest_fp_tag(&mut runbook)?;
        upsert_runbook(&mut notebook.runbooks, runbook)?;
        touched_runbook_ids.insert(incoming.id.clone());
        if existed {
            counts.updated_runbooks += 1;
        } else {
            counts.new_runbooks += 1;
        }
        if counts.changes.len() < 50 {
            counts.changes.push(NotebookApplySuggestChange {
                kind: NotebookApplySuggestChangeKind::Runbook,
                id: incoming.id.clone(),
                action: if existed {
                    NotebookApplySuggestChangeAction::Update
                } else {
                    NotebookApplySuggestChangeAction::Create
                },
                reason: None,
                hint: change_hint,
            });
        } else {
            counts.changes_truncated = true;
        }
    }

    counts.touched_anchor_ids = touched_anchor_ids.into_iter().collect();
    counts.touched_runbook_ids = touched_runbook_ids.into_iter().collect();
    counts.touched_anchor_ids.sort();
    counts.touched_runbook_ids.sort();
    Ok(counts)
}

fn extract_suggest_fp(tags: &[String]) -> Option<&str> {
    tags.iter()
        .find_map(|t| t.strip_prefix(SUGGEST_FP_TAG_PREFIX))
}

#[derive(Debug, Serialize)]
struct AnchorFingerprintEvidence {
    file: String,
    start_line: u32,
    end_line: u32,
}

#[derive(Debug, Serialize)]
struct AnchorFingerprintLocator {
    kind: super::notebook_types::NotebookLocatorKind,
    value: String,
}

#[derive(Debug, Serialize)]
struct AnchorFingerprint {
    id: String,
    kind: super::notebook_types::NotebookAnchorKind,
    label: String,
    evidence: Vec<AnchorFingerprintEvidence>,
    locator: Option<AnchorFingerprintLocator>,
    tags: Vec<String>,
}

fn compute_anchor_fingerprint(anchor: &NotebookAnchor) -> Result<String> {
    let mut tags: Vec<String> = anchor
        .tags
        .iter()
        .filter(|t| !t.starts_with(SUGGEST_FP_TAG_PREFIX))
        .cloned()
        .collect();
    tags.sort();

    let mut evidence: Vec<AnchorFingerprintEvidence> = anchor
        .evidence
        .iter()
        .map(|ev| AnchorFingerprintEvidence {
            file: ev.file.replace('\\', "/"),
            start_line: ev.start_line,
            end_line: ev.end_line,
        })
        .collect();
    evidence.sort_by(|a, b| {
        (&a.file, a.start_line, a.end_line).cmp(&(&b.file, b.start_line, b.end_line))
    });

    let locator = anchor.locator.as_ref().map(|loc| AnchorFingerprintLocator {
        kind: loc.kind.clone(),
        value: loc.value.clone(),
    });

    let fp = AnchorFingerprint {
        id: anchor.id.clone(),
        kind: anchor.kind.clone(),
        label: anchor.label.clone(),
        evidence,
        locator,
        tags,
    };

    let bytes = serde_json::to_vec(&fp).context("serialize anchor fingerprint")?;
    let digest = Sha256::digest(&bytes);
    Ok(hex_encode_lower(&digest))
}

fn set_suggest_fp_tag(anchor: &mut NotebookAnchor) -> Result<()> {
    let fp = compute_anchor_fingerprint(anchor)?;
    anchor
        .tags
        .retain(|t| !t.starts_with(SUGGEST_FP_TAG_PREFIX));
    anchor.tags.push(format!("{SUGGEST_FP_TAG_PREFIX}{fp}"));
    Ok(())
}

#[derive(Debug, Serialize)]
struct RunbookFingerprint {
    id: String,
    title: String,
    purpose: String,
    policy: RunbookPolicy,
    sections: Vec<RunbookSection>,
    tags: Vec<String>,
}

fn compute_runbook_fingerprint(runbook: &AgentRunbook) -> Result<String> {
    let mut tags: Vec<String> = runbook
        .tags
        .iter()
        .filter(|t| !t.starts_with(SUGGEST_FP_TAG_PREFIX))
        .cloned()
        .collect();
    tags.sort();

    let fp = RunbookFingerprint {
        id: runbook.id.clone(),
        title: runbook.title.clone(),
        purpose: runbook.purpose.clone(),
        policy: runbook.policy.clone(),
        sections: runbook.sections.clone(),
        tags,
    };

    let bytes = serde_json::to_vec(&fp).context("serialize runbook fingerprint")?;
    let digest = Sha256::digest(&bytes);
    Ok(hex_encode_lower(&digest))
}

fn set_runbook_suggest_fp_tag(runbook: &mut AgentRunbook) -> Result<()> {
    let fp = compute_runbook_fingerprint(runbook)?;
    runbook
        .tags
        .retain(|t| !t.starts_with(SUGGEST_FP_TAG_PREFIX));
    runbook.tags.push(format!("{SUGGEST_FP_TAG_PREFIX}{fp}"));
    Ok(())
}

fn should_skip_anchor(existing: &NotebookAnchor) -> Result<Option<NotebookApplySuggestSkipReason>> {
    let Some(stored_fp) = extract_suggest_fp(&existing.tags) else {
        return Ok(Some(NotebookApplySuggestSkipReason::NotManaged));
    };
    let actual_fp = compute_anchor_fingerprint(existing)?;
    if actual_fp != stored_fp {
        return Ok(Some(NotebookApplySuggestSkipReason::ManualModified));
    }
    Ok(None)
}

fn should_skip_runbook(existing: &AgentRunbook) -> Result<Option<NotebookApplySuggestSkipReason>> {
    let Some(stored_fp) = extract_suggest_fp(&existing.tags) else {
        return Ok(Some(NotebookApplySuggestSkipReason::NotManaged));
    };
    let actual_fp = compute_runbook_fingerprint(existing)?;
    if actual_fp != stored_fp {
        return Ok(Some(NotebookApplySuggestSkipReason::ManualModified));
    }
    Ok(None)
}

fn skip_change_hint(reason: &NotebookApplySuggestSkipReason) -> Option<String> {
    let hint = match reason {
        NotebookApplySuggestSkipReason::NotManaged => "not managed (use overwrite_policy=force)",
        NotebookApplySuggestSkipReason::ManualModified => {
            "manual edits detected (use overwrite_policy=force)"
        }
    };
    Some(hint.to_string())
}

fn truncate_hint(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        return s.to_string();
    }
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i + 1 >= max_len {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

fn anchor_change_hint(
    existing: Option<&NotebookAnchor>,
    incoming: &NotebookAnchor,
) -> Option<String> {
    let existing = existing?;
    let mut parts: Vec<String> = Vec::new();
    if existing.label != incoming.label {
        parts.push(format!(
            "label: {} -> {}",
            truncate_hint(&existing.label, 32),
            truncate_hint(&incoming.label, 32)
        ));
    }
    if existing.kind != incoming.kind {
        parts.push(format!("kind: {:?} -> {:?}", existing.kind, incoming.kind));
    }

    let existing_keys: HashSet<(String, u32, u32)> = existing
        .evidence
        .iter()
        .map(|ev| (ev.file.replace('\\', "/"), ev.start_line, ev.end_line))
        .collect();
    let incoming_keys: HashSet<(String, u32, u32)> = incoming
        .evidence
        .iter()
        .map(|ev| (ev.file.replace('\\', "/"), ev.start_line, ev.end_line))
        .collect();
    if existing_keys != incoming_keys {
        parts.push(format!(
            "evidence: {} -> {}",
            existing_keys.len(),
            incoming_keys.len()
        ));
    }

    let existing_loc = existing
        .locator
        .as_ref()
        .map(|l| (&l.kind, l.value.as_str()));
    let incoming_loc = incoming
        .locator
        .as_ref()
        .map(|l| (&l.kind, l.value.as_str()));
    if existing_loc != incoming_loc {
        parts.push("locator: changed".to_string());
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

fn anchor_create_hint(anchor: &NotebookAnchor) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("label: {}", truncate_hint(&anchor.label, 32)));
    parts.push(format!("kind: {:?}", anchor.kind));
    parts.push(format!("evidence: {}", anchor.evidence.len()));
    if let Some(locator) = anchor.locator.as_ref() {
        parts.push(format!(
            "locator: {:?} {}",
            locator.kind,
            truncate_hint(&locator.value, 32)
        ));
    }
    Some(parts.join("; "))
}

fn runbook_create_hint(runbook: &AgentRunbook) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("title: {}", truncate_hint(&runbook.title, 32)));
    if !runbook.purpose.trim().is_empty() {
        parts.push("purpose: set".to_string());
    }
    parts.push(format!("sections: {}", runbook.sections.len()));
    Some(parts.join("; "))
}

fn runbook_change_hint(existing: Option<&AgentRunbook>, incoming: &AgentRunbook) -> Option<String> {
    let existing = existing?;
    let mut parts: Vec<String> = Vec::new();
    if existing.title != incoming.title {
        parts.push(format!(
            "title: {} -> {}",
            truncate_hint(&existing.title, 32),
            truncate_hint(&incoming.title, 32)
        ));
    }
    if existing.purpose != incoming.purpose {
        parts.push("purpose: changed".to_string());
    }
    if existing.policy.default_mode != incoming.policy.default_mode {
        parts.push(format!(
            "default_mode: {:?} -> {:?}",
            existing.policy.default_mode, incoming.policy.default_mode
        ));
    }
    if (existing.policy.noise_budget - incoming.policy.noise_budget).abs() > f32::EPSILON {
        parts.push("noise_budget: changed".to_string());
    }
    if existing.policy.max_items_per_section != incoming.policy.max_items_per_section {
        parts.push(format!(
            "max_items_per_section: {} -> {}",
            existing.policy.max_items_per_section, incoming.policy.max_items_per_section
        ));
    }

    if existing.sections.len() != incoming.sections.len() {
        parts.push(format!(
            "sections: {} -> {}",
            existing.sections.len(),
            incoming.sections.len()
        ));
    } else {
        let old = serde_json::to_vec(&existing.sections).ok();
        let new = serde_json::to_vec(&incoming.sections).ok();
        if old != new {
            parts.push("sections: changed".to_string());
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

fn ensure_unique_ids<'a, I>(ids: I, label: &str) -> Result<()>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut seen = HashSet::new();
    let mut dups = Vec::new();
    for id in ids {
        if !seen.insert(id) {
            dups.push(id.to_string());
        }
    }
    if !dups.is_empty() {
        dups.sort();
        anyhow::bail!(
            "Suggestion contains duplicate {label} ids: {}",
            dups.join(", ")
        );
    }
    Ok(())
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
        validate_evidence_pointer(root, ev)?;
    }
    Ok(())
}

fn validate_evidence_pointer(root: &Path, ev: &NotebookEvidencePointer) -> Result<()> {
    let file = ev.file.trim();
    if file.is_empty() {
        anyhow::bail!("Anchor evidence file must not be empty");
    }
    if Path::new(file).is_absolute() {
        anyhow::bail!("Anchor evidence file must be repo-relative (got absolute path)");
    }
    let rel = file.replace('\\', "/");
    if super::secrets::is_potential_secret_path(&rel) {
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

fn preserve_existing_source_hashes(existing: &NotebookAnchor, incoming: &mut NotebookAnchor) {
    let mut by_key: HashMap<(String, u32, u32), Option<String>> = HashMap::new();
    for ptr in &existing.evidence {
        let key = (ptr.file.replace('\\', "/"), ptr.start_line, ptr.end_line);
        by_key.insert(key, ptr.source_hash.clone());
    }
    for ptr in &mut incoming.evidence {
        if ptr.source_hash.is_some() {
            continue;
        }
        let key = (ptr.file.replace('\\', "/"), ptr.start_line, ptr.end_line);
        if let Some(hash) = by_key.get(&key).and_then(|v| v.clone()) {
            ptr.source_hash = Some(hash);
        }
    }
}

fn upsert_anchor(anchors: &mut Vec<NotebookAnchor>, anchor: NotebookAnchor) -> Result<()> {
    if let Some(existing) = anchors.iter_mut().find(|a| a.id == anchor.id) {
        *existing = anchor;
        return Ok(());
    }
    anchors.push(anchor);
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

fn generate_backup_id(bytes: &[u8]) -> Result<String> {
    let now = unix_ms(SystemTime::now());
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let hex = hex_encode_lower(&digest);
    let short = hex
        .get(0..12)
        .ok_or_else(|| anyhow!("failed to compute backup id"))?;
    Ok(format!("{now}-{short}"))
}

fn backup_path_for_id(notebook_path: &Path, backup_id: &str) -> PathBuf {
    let dir = notebook_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join(format!("notebook_v1.backup-{backup_id}.json"))
}

fn unique_backup_path(notebook_path: &Path, backup_id: &str) -> Result<(String, PathBuf)> {
    let candidate = backup_path_for_id(notebook_path, backup_id);
    if !candidate.exists() {
        return Ok((backup_id.to_string(), candidate));
    }
    for i in 1..=1000u32 {
        let id = format!("{backup_id}-{i}");
        let candidate = backup_path_for_id(notebook_path, &id);
        if !candidate.exists() {
            return Ok((id, candidate));
        }
    }
    anyhow::bail!("failed to allocate unique backup path (too many collisions)");
}

fn cleanup_old_backups(notebook_path: &Path, max_backups: u32) -> Result<()> {
    if max_backups == 0 {
        return Ok(());
    }
    let Some(dir) = notebook_path.parent() else {
        return Ok(());
    };
    let prefix = "notebook_v1.backup-";
    let mut backups: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("read notebook dir {}", dir.display()))?
    {
        let entry = match entry {
            Ok(v) => v,
            Err(_) => continue,
        };
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !name.starts_with(prefix) || !name.ends_with(".json") {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        backups.push((mtime, entry.path()));
    }
    backups.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    let keep = usize::try_from(max_backups).unwrap_or(0);
    for (_, path) in backups.into_iter().skip(keep) {
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("backup path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create backup dir {}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("notebook_backup"),
        std::process::id()
    ));

    {
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("create tmp {}", tmp.display()))?;
        use std::io::Write as _;
        file.write_all(bytes)
            .with_context(|| format!("write tmp {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync tmp {}", tmp.display()))?;
    }

    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename tmp {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}
