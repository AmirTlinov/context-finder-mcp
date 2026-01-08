use crate::command::context::{index_path, load_store_mtime, unix_ms};
use crate::command::domain::{Hint, HintKind, RequestOptions, StalePolicy};
use anyhow::Result;
use context_indexer::{
    assess_staleness, compute_project_watermark, read_index_watermark, IndexSnapshot, IndexState,
    IndexerError, PersistedIndexWatermark, ProjectIndexer, ReindexAttempt, ReindexResult,
    StaleReason, Watermark, INDEX_STATE_SCHEMA_VERSION,
};
use context_search::SearchProfile;
use context_vector_store::current_model_id;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct FreshnessGate {
    pub index_state: IndexState,
    pub hints: Vec<Hint>,
    pub index_updated: bool,
}

#[derive(Debug)]
pub struct FreshnessBlock {
    pub message: String,
    pub hints: Vec<Hint>,
    pub index_state: IndexState,
}

pub fn action_requires_index(action: &crate::command::domain::CommandAction) -> bool {
    use crate::command::domain::CommandAction as A;
    matches!(
        *action,
        A::Search
            | A::SearchWithContext
            | A::ContextPack
            | A::TaskPack
            | A::CompareSearch
            | A::Map
            | A::Eval
            | A::EvalCompare
    )
}

pub fn extract_project_path(payload: &serde_json::Value) -> Option<PathBuf> {
    payload
        .get("project")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .or_else(|| {
            payload
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
        })
}

pub async fn gather_index_state(project_root: &Path, profile_name: &str) -> Result<IndexState> {
    let project_watermark = compute_project_watermark(project_root).await?;
    gather_index_state_with_project_mark(project_root, profile_name, project_watermark).await
}

async fn gather_index_state_with_project_mark(
    project_root: &Path,
    profile_name: &str,
    project_watermark: Watermark,
) -> Result<IndexState> {
    let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    let store_path = index_path(project_root);
    let index_exists = store_path.exists();

    let mut index_corrupt = false;
    let mut index_mtime_ms = None;
    if index_exists {
        match load_store_mtime(&store_path).await {
            Ok(mtime) => {
                index_mtime_ms = Some(unix_ms(mtime));
            }
            Err(_) => {
                index_corrupt = true;
            }
        }
    }

    let mut watermark = None;
    let mut built_at_unix_ms = None;
    match read_index_watermark(&store_path).await {
        Ok(Some(PersistedIndexWatermark {
            built_at_unix_ms: built_at,
            watermark: mark,
        })) => {
            built_at_unix_ms = Some(built_at);
            watermark = Some(mark);
        }
        Ok(None) => {}
        Err(_) => {
            index_corrupt = true;
        }
    }

    let assessment = assess_staleness(
        &project_watermark,
        index_exists,
        index_corrupt,
        watermark.as_ref(),
    );

    let snapshot = IndexSnapshot {
        exists: index_exists,
        path: Some(store_path.display().to_string()),
        mtime_ms: index_mtime_ms,
        built_at_unix_ms,
        watermark,
    };

    Ok(IndexState {
        schema_version: INDEX_STATE_SCHEMA_VERSION,
        project_root: Some(project_root.display().to_string()),
        model_id,
        profile: profile_name.to_string(),
        project_watermark,
        index: snapshot,
        stale: assessment.stale,
        stale_reasons: assessment.reasons,
        reindex: None,
    })
}

pub async fn enforce_stale_policy(
    project_root: &Path,
    profile_name: &str,
    profile: &SearchProfile,
    options: &RequestOptions,
) -> Result<std::result::Result<FreshnessGate, FreshnessBlock>> {
    let project_mark = compute_project_watermark(project_root).await?;
    let mut gate = FreshnessGate {
        index_state: gather_index_state_with_project_mark(project_root, profile_name, project_mark)
            .await?,
        hints: Vec::new(),
        index_updated: false,
    };

    match options.stale_policy {
        StalePolicy::Auto => {
            if gate.index_state.stale || !gate.index_state.index.exists {
                let attempt = attempt_reindex(project_root, profile, options.max_reindex_ms).await;
                gate.hints.push(render_reindex_hint(&attempt));
                gate.index_updated |= attempt.performed;

                if let Ok(refreshed) = gather_index_state(project_root, profile_name).await {
                    gate.index_state = refreshed;
                }
                gate.index_state.reindex = Some(attempt);
            }

            if !gate.index_state.index.exists {
                return Ok(Err(FreshnessBlock {
                    message: missing_index_message(&gate.index_state),
                    hints: gate.hints,
                    index_state: gate.index_state,
                }));
            }

            if gate.index_state.stale {
                gate.hints.push(Hint {
                    kind: HintKind::Warn,
                    text: format!(
                        "Index appears stale ({}). Proceeding due to stale_policy=auto.",
                        format_stale_reasons(&gate.index_state.stale_reasons)
                    ),
                });
            }
        }
        StalePolicy::Warn => {
            if !gate.index_state.index.exists {
                return Ok(Err(FreshnessBlock {
                    message: missing_index_message(&gate.index_state),
                    hints: gate.hints,
                    index_state: gate.index_state,
                }));
            }
            if gate.index_state.stale {
                gate.hints.push(Hint {
                    kind: HintKind::Warn,
                    text: format!(
                        "Index appears stale ({}). Proceeding due to stale_policy=warn.",
                        format_stale_reasons(&gate.index_state.stale_reasons)
                    ),
                });
            }
        }
        StalePolicy::Fail => {
            if !gate.index_state.index.exists {
                return Ok(Err(FreshnessBlock {
                    message: missing_index_message(&gate.index_state),
                    hints: gate.hints,
                    index_state: gate.index_state,
                }));
            }
            if gate.index_state.stale {
                return Ok(Err(FreshnessBlock {
                    message: format!(
                        "Index is stale ({}). Rebuild it or set options.stale_policy to 'warn'/'auto'.",
                        format_stale_reasons(&gate.index_state.stale_reasons)
                    ),
                    hints: gate.hints,
                    index_state: gate.index_state,
                }));
            }
        }
    }

    Ok(Ok(gate))
}

pub async fn attempt_reindex(
    project_root: &Path,
    profile: &SearchProfile,
    max_reindex_ms: u64,
) -> ReindexAttempt {
    let start = Instant::now();
    let budget = Duration::from_millis(max_reindex_ms);

    let mut attempt = ReindexAttempt {
        attempted: true,
        performed: false,
        budget_ms: Some(max_reindex_ms),
        duration_ms: None,
        result: None,
        error: None,
    };

    let templates = profile.embedding().clone();
    let indexer = match ProjectIndexer::new_with_embedding_templates(project_root, templates).await
    {
        Ok(i) => i,
        Err(err) => {
            attempt.duration_ms = Some(start.elapsed().as_millis() as u64);
            attempt.result = Some(ReindexResult::Failed);
            attempt.error = Some(err.to_string());
            return attempt;
        }
    };

    match indexer.index_with_budget(budget).await {
        Ok(_) => {
            attempt.performed = true;
            attempt.result = Some(ReindexResult::Ok);
        }
        Err(IndexerError::BudgetExceeded) => {
            attempt.result = Some(ReindexResult::BudgetExceeded);
        }
        Err(err) => {
            attempt.result = Some(ReindexResult::Failed);
            attempt.error = Some(err.to_string());
        }
    }

    attempt.duration_ms = Some(start.elapsed().as_millis() as u64);
    attempt
}

pub fn render_reindex_hint(attempt: &ReindexAttempt) -> Hint {
    let budget = attempt
        .budget_ms
        .map(|v| format!("{v}ms"))
        .unwrap_or_else(|| "unknown".to_string());
    let duration = attempt
        .duration_ms
        .map(|v| format!("{v}ms"))
        .unwrap_or_else(|| "unknown".to_string());

    match attempt.result {
        Some(ReindexResult::Ok) => Hint {
            kind: HintKind::Cache,
            text: format!("Auto reindex OK in {duration} (budget {budget})"),
        },
        Some(ReindexResult::BudgetExceeded) => Hint {
            kind: HintKind::Warn,
            text: format!("Auto reindex exceeded budget {budget} (ran {duration})"),
        },
        Some(ReindexResult::Failed) => Hint {
            kind: HintKind::Warn,
            text: format!(
                "Auto reindex failed in {duration} (budget {budget}): {}",
                attempt.error.as_deref().unwrap_or("unknown error")
            ),
        },
        Some(ReindexResult::Skipped) | None => Hint {
            kind: HintKind::Info,
            text: "Auto reindex skipped".to_string(),
        },
    }
}

fn missing_index_message(state: &IndexState) -> String {
    let path = state
        .index
        .path
        .as_deref()
        .unwrap_or("<unknown-index-path>");
    format!("Index not found at {path}. Run 'context index' first.")
}

fn stale_reason_name(reason: &StaleReason) -> &'static str {
    match reason {
        StaleReason::IndexMissing => "index_missing",
        StaleReason::IndexCorrupt => "index_corrupt",
        StaleReason::WatermarkMissing => "watermark_missing",
        StaleReason::GitHeadMismatch => "git_head_mismatch",
        StaleReason::GitDirtyMismatch => "git_dirty_mismatch",
        StaleReason::FilesystemChanged => "filesystem_changed",
    }
}

fn format_stale_reasons(reasons: &[StaleReason]) -> String {
    if reasons.is_empty() {
        return "unknown".to_string();
    }
    reasons
        .iter()
        .map(stale_reason_name)
        .collect::<Vec<_>>()
        .join(", ")
}
