use super::super::{
    load_corpus_chunk_ids, load_index_chunk_ids, load_model_statuses, sample_file_paths,
    CallToolResult, Content, ContextFinderService, DoctorEnvResult, DoctorIndexDrift,
    DoctorIndexingObservability, DoctorObservability, DoctorProjectResult, DoctorRequest,
    DoctorResult, DoctorWarmIndexersObservability, McpError, ResponseMode, ToolMeta,
};
use crate::runtime_env;
use crate::tools::context_doc::ContextDocBuilder;
use context_protocol::ToolNextAction;
use context_vector_store::context_dir_for_project_root;
use context_vector_store::corpus_path_for_project_root;
use context_vector_store::current_model_id;
use context_vector_store::QueryKind;
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;

use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_request_with_root_context,
    meta_for_request,
};

fn active_semantic_models(service: &ContextFinderService) -> HashSet<String> {
    // Agent-native behavior: drift should be reported only for models that can actually affect
    // the current profile / request path. Keeping drift checks scoped avoids false alarms when
    // indexes for unused models are present on disk (e.g., from a previous profile run).
    let mut out = HashSet::new();
    let experts = service.profile.experts();
    for kind in [
        QueryKind::Identifier,
        QueryKind::Path,
        QueryKind::Conceptual,
    ] {
        for model_id in experts.semantic_models(kind) {
            out.insert(model_id.clone());
        }
    }
    if let Ok(primary) = current_model_id() {
        out.insert(primary);
    }
    out
}

async fn diagnose_project(
    root: &Path,
    active_models: &HashSet<String>,
    response_mode: ResponseMode,
    issues: &mut Vec<String>,
    hints: &mut Vec<String>,
) -> Option<DoctorProjectResult> {
    let corpus_path = corpus_path_for_project_root(root);
    let has_corpus = corpus_path.exists();

    let indexes_dir = context_dir_for_project_root(root).join("indexes");
    let mut indexed_models: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&indexes_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let index_path = path.join("index.json");
            if index_path.exists() {
                indexed_models.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    indexed_models.sort();

    if indexed_models.is_empty() {
        hints.push(
            "No semantic indexes found for this project yet. Semantic tools (search/context/context_pack/impact/trace) will warm the index automatically on demand.".into(),
        );
    }

    let mut drift: Vec<DoctorIndexDrift> = Vec::new();
    if has_corpus && !indexed_models.is_empty() {
        match load_corpus_chunk_ids(&corpus_path).await {
            Ok(corpus_ids) => {
                let corpus_chunks = corpus_ids.len();
                let mut drifted_models = Vec::new();

                for model_id in &indexed_models {
                    let index_path = indexes_dir.join(model_id).join("index.json");
                    let index_ids = match load_index_chunk_ids(&index_path).await {
                        Ok(ids) => ids,
                        Err(err) => {
                            issues.push(format!(
                                "Failed to read index for model '{model_id}': {err:#}"
                            ));
                            continue;
                        }
                    };

                    let missing_chunks = corpus_ids.difference(&index_ids).count();
                    let extra_chunks = index_ids.difference(&corpus_ids).count();

                    if (missing_chunks > 0 || extra_chunks > 0) && active_models.contains(model_id)
                    {
                        drifted_models.push(model_id.clone());
                    }

                    let missing_file_samples =
                        sample_file_paths(corpus_ids.difference(&index_ids), 8);
                    let extra_file_samples =
                        sample_file_paths(index_ids.difference(&corpus_ids), 8);

                    drift.push(DoctorIndexDrift {
                        model: model_id.clone(),
                        index_path: index_path.to_string_lossy().into_owned(),
                        index_chunks: index_ids.len(),
                        corpus_chunks,
                        missing_chunks,
                        extra_chunks,
                        missing_file_samples,
                        extra_file_samples,
                    });
                }

                if response_mode == ResponseMode::Full && !drifted_models.is_empty() {
                    hints.push(format!(
                        "Index drift detected vs corpus for active models: {}",
                        drifted_models.join(", ")
                    ));
                    let root_display = root.to_string_lossy().to_string();
                    let models_csv = drifted_models.join(",");
                    hints.push(format!(
                        "If it doesn't self-heal, force a rebuild: `context index {root_display} --force --models {models_csv}`."
                    ));
                }
            }
            Err(err) => {
                issues.push(format!(
                    "Failed to load corpus {}: {err:#}",
                    corpus_path.display()
                ));
            }
        }
    } else if !has_corpus && !indexed_models.is_empty() {
        hints.push("Corpus not found for this project; drift detection is unavailable. Any semantic tool call will regenerate corpus + indexes automatically on demand.".into());
    }

    Some(DoctorProjectResult {
        root: root.to_string_lossy().into_owned(),
        corpus_path: corpus_path.to_string_lossy().into_owned(),
        has_corpus,
        indexed_models,
        drift,
    })
}

async fn compute_observability(service: &ContextFinderService) -> DoctorObservability {
    let indexing = context_indexer::index_concurrency_snapshot();
    let write_lock_wait_ms_last = context_indexer::index_write_lock_wait_ms_last();
    let write_lock_wait_ms_max = context_indexer::index_write_lock_wait_ms_max();

    let warm = {
        let guard = service.state.warm_indexes.lock().await;
        guard.snapshot()
    };

    DoctorObservability {
        indexing: DoctorIndexingObservability {
            concurrency_limit: indexing.limit,
            concurrency_in_flight: indexing.in_flight,
            concurrency_waiters: indexing.waiters,
            write_lock_wait_ms_last,
            write_lock_wait_ms_max,
        },
        warm_indexers: DoctorWarmIndexersObservability {
            workers: warm.workers,
            starting: warm.starting,
            lru: warm.lru,
        },
    }
}

/// Diagnose model/GPU/index configuration
pub(in crate::tools::dispatch) async fn doctor(
    service: &ContextFinderService,
    request: DoctorRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let DoctorRequest { path, .. } = request;
    let model_dir = context_vector_store::model_dir();
    let manifest_path = model_dir.join("manifest.json");

    let (model_manifest_exists, models) = match load_model_statuses(&model_dir).await {
        Ok(result) => result,
        Err(err) => {
            let meta = if response_mode == ResponseMode::Minimal {
                ToolMeta::default()
            } else {
                meta_for_request(service, path.as_deref()).await
            };
            return Ok(internal_error_with_meta(
                format!(
                    "Failed to load model manifest {}: {err:#}",
                    manifest_path.display()
                ),
                meta,
            ));
        }
    };

    let gpu = runtime_env::diagnose_gpu_env();
    let cuda_disabled = runtime_env::is_cuda_disabled();
    let allow_cpu_fallback = std::env::var("CONTEXT_ALLOW_CPU")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let mut issues: Vec<String> = Vec::new();
    let mut hints: Vec<String> = Vec::new();

    if !cuda_disabled && (!gpu.provider_present || !gpu.cublas_present) {
        issues.push("CUDA libraries are not fully configured (provider/cublas missing).".into());
        hints.push("Run `bash scripts/setup_cuda_deps.sh` in the Context repo, or set ORT_LIB_LOCATION/LD_LIBRARY_PATH to directories containing libonnxruntime_providers_cuda.so and libcublasLt.so.*. If you want CPU fallback, set CONTEXT_ALLOW_CPU=1.".into());
    }

    if !model_manifest_exists {
        issues.push(format!(
            "Model manifest not found at {}",
            manifest_path.display()
        ));
        hints.push("Run `context install-models` (or set CONTEXT_MODEL_DIR to a directory containing models/manifest.json).".into());
    } else if models.iter().any(|m| !m.installed) {
        hints.push("Some models are missing assets. Run `context install-models` to download them into the model directory.".into());
    }

    let (root, root_display) = match service
        .resolve_root_no_daemon_touch_for_tool(path.as_deref(), "doctor")
        .await
    {
        Ok(value) => value,
        Err(message) => {
            let meta = if response_mode == ResponseMode::Minimal {
                ToolMeta::default()
            } else {
                meta_for_request(service, path.as_deref()).await
            };
            return Ok(
                invalid_request_with_root_context(service, message, meta, None, Vec::new()).await,
            );
        }
    };
    let meta = service.tool_meta(&root).await;
    let meta_for_output = if response_mode == ResponseMode::Minimal {
        ToolMeta {
            root_fingerprint: meta.root_fingerprint,
            ..ToolMeta::default()
        }
    } else {
        meta
    };
    let active_models = active_semantic_models(service);
    let project = diagnose_project(
        &root,
        &active_models,
        response_mode,
        &mut issues,
        &mut hints,
    )
    .await;
    let observability = if response_mode == ResponseMode::Full {
        Some(compute_observability(service).await)
    } else {
        None
    };

    let mut result = DoctorResult {
        env: DoctorEnvResult {
            profile: service.profile.name().to_string(),
            model_dir: model_dir.to_string_lossy().into_owned(),
            model_manifest_exists,
            models,
            gpu,
            cuda_disabled,
            allow_cpu_fallback,
        },
        project,
        issues,
        hints,
        next_actions: Vec::new(),
        observability,
        meta: meta_for_output,
    };
    if response_mode == ResponseMode::Full {
        if let Some(project) = result.project.as_ref() {
            let budgets = super::super::mcp_default_budgets();
            if !project.has_corpus || project.indexed_models.is_empty() {
                result.next_actions.push(ToolNextAction {
                    tool: "search".to_string(),
                    args: json!({
                        "path": root_display.clone(),
                        "query": "main entry point / architecture overview",
                    }),
                    reason: "Trigger auto-index and verify semantic retrieval; search falls back to grep while the index warms.".to_string(),
                });
            }
            result.next_actions.push(ToolNextAction {
                tool: "repo_onboarding_pack".to_string(),
                args: json!({
                    "path": root_display.clone(),
                    "max_chars": budgets.repo_onboarding_pack_max_chars
                }),
                reason: "Get a compact repo map + key docs for fast onboarding.".to_string(),
            });
            if project.has_corpus {
                result.next_actions.push(ToolNextAction {
                    tool: "context_pack".to_string(),
                    args: json!({
                        "path": root_display.clone(),
                        "query": "project overview",
                        "max_chars": budgets.context_pack_max_chars
                    }),
                    reason: "Build a bounded semantic overview after diagnostics.".to_string(),
                });
            }
        }
    }

    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!(
        "doctor: issues={} hints={}",
        result.issues.len(),
        result.hints.len()
    ));
    if response_mode != ResponseMode::Minimal {
        doc.push_root_fingerprint(result.meta.root_fingerprint);
    }
    if response_mode == ResponseMode::Full {
        doc.push_note(&format!("profile: {}", result.env.profile));
        doc.push_note(&format!(
            "cuda_disabled={} allow_cpu_fallback={}",
            result.env.cuda_disabled, result.env.allow_cpu_fallback
        ));
        if let Some(obs) = result.observability.as_ref() {
            doc.push_note(&format!(
                "indexing: limit={} in_flight={} waiters={} write_lock_wait_ms(last/max)={}/{}",
                obs.indexing.concurrency_limit,
                obs.indexing.concurrency_in_flight,
                obs.indexing.concurrency_waiters,
                obs.indexing.write_lock_wait_ms_last,
                obs.indexing.write_lock_wait_ms_max
            ));
            doc.push_note(&format!(
                "warm_indexers: workers={} starting={} lru={}",
                obs.warm_indexers.workers, obs.warm_indexers.starting, obs.warm_indexers.lru
            ));
        }
    }
    if !result.issues.is_empty() {
        doc.push_note("issues:");
        for issue in &result.issues {
            doc.push_line(&format!("- {issue}"));
        }
    }
    if !result.hints.is_empty() {
        doc.push_note("hints:");
        for hint in &result.hints {
            doc.push_line(&format!("- {hint}"));
        }
    }
    if let Some(project) = result.project.as_ref() {
        doc.push_note(&format!("project_root: {}", project.root));
        doc.push_note(&format!(
            "has_corpus={} indexed_models={}",
            project.has_corpus,
            project.indexed_models.len()
        ));
    }
    let output = CallToolResult::success(vec![Content::text(doc.finish())]);
    Ok(attach_structured_content(
        output,
        &result,
        result.meta.clone(),
        "doctor",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_MUTEX;
    use context_code_chunker::ChunkMetadata;
    use context_vector_store::ChunkCorpus;
    use std::collections::HashSet;

    fn chunk(file: &str, start: usize, end: usize) -> context_code_chunker::CodeChunk {
        context_code_chunker::CodeChunk::new(
            file.to_string(),
            start,
            end,
            "fn x() {}\n".to_string(),
            ChunkMetadata::default(),
        )
    }

    #[tokio::test]
    async fn drift_hints_suggest_models_reindex_command_for_active_models_in_full_mode() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        let mut corpus = ChunkCorpus::new();
        corpus.set_file_chunks("a.rs".to_string(), vec![chunk("a.rs", 1, 2)]);
        corpus
            .save(corpus_path_for_project_root(root))
            .await
            .expect("save corpus");

        let index_dir = context_dir_for_project_root(root)
            .join("indexes")
            .join("embeddinggemma-300m");
        std::fs::create_dir_all(&index_dir).expect("mkdir index dir");
        std::fs::write(
            index_dir.join("index.json"),
            r#"{"schema_version":3,"id_map":{"0":"b.rs:1:1"}}"#,
        )
        .expect("write index.json");

        let mut issues = Vec::new();
        let mut hints = Vec::new();
        let active_models = HashSet::from(["embeddinggemma-300m".to_string()]);
        diagnose_project(
            root,
            &active_models,
            ResponseMode::Full,
            &mut issues,
            &mut hints,
        )
        .await
        .expect("project result");

        assert!(
            issues.is_empty(),
            "expected drift not to be an issue (issues={issues:?})"
        );
        assert!(
            hints
                .iter()
                .any(|v| v.contains("--models embeddinggemma-300m")),
            "expected hint to suggest targeted --models reindex (hints={hints:?})"
        );
    }

    #[tokio::test]
    async fn drift_for_inactive_models_is_suppressed_in_default_mode() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        let mut corpus = ChunkCorpus::new();
        corpus.set_file_chunks("a.rs".to_string(), vec![chunk("a.rs", 1, 2)]);
        corpus
            .save(corpus_path_for_project_root(root))
            .await
            .expect("save corpus");

        let index_dir = context_dir_for_project_root(root)
            .join("indexes")
            .join("embeddinggemma-300m");
        std::fs::create_dir_all(&index_dir).expect("mkdir index dir");
        std::fs::write(
            index_dir.join("index.json"),
            r#"{"schema_version":3,"id_map":{"0":"b.rs:1:1"}}"#,
        )
        .expect("write index.json");

        let active_models: HashSet<String> = HashSet::new();
        let mut issues = Vec::new();
        let mut hints = Vec::new();
        diagnose_project(
            root,
            &active_models,
            ResponseMode::Facts,
            &mut issues,
            &mut hints,
        )
        .await
        .expect("project result");

        assert!(
            issues.is_empty(),
            "expected no issues for drifted inactive model (issues={issues:?})"
        );
        assert!(
            !hints.iter().any(|v| v.contains("Index drift detected")),
            "expected drift hint to be suppressed for inactive model (hints={hints:?})"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn doctor_full_does_not_emit_index_next_action() {
        let _env = ENV_MUTEX.lock().expect("ENV_MUTEX");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        let mut corpus = ChunkCorpus::new();
        corpus.set_file_chunks("a.rs".to_string(), vec![chunk("a.rs", 1, 2)]);
        corpus
            .save(corpus_path_for_project_root(root))
            .await
            .expect("save corpus");

        let index_dir = context_dir_for_project_root(root)
            .join("indexes")
            .join("embeddinggemma-300m");
        std::fs::create_dir_all(&index_dir).expect("mkdir index dir");
        std::fs::write(
            index_dir.join("index.json"),
            r#"{"schema_version":3,"id_map":{"0":"b.rs:1:1"}}"#,
        )
        .expect("write index.json");

        let service = ContextFinderService::new();
        let request = DoctorRequest {
            path: Some(root.to_string_lossy().to_string()),
            response_mode: Some(ResponseMode::Full),
        };

        let output = doctor(&service, request).await.expect("doctor");
        let structured = output
            .structured_content
            .as_ref()
            .expect("structured content");
        let next_actions = structured
            .get("next_actions")
            .and_then(|v| v.as_array())
            .unwrap_or_else(|| panic!("next_actions array (structured={structured:?})"));

        assert!(
            !next_actions.iter().any(|action| action
                .get("tool")
                .and_then(|v| v.as_str())
                .is_some_and(|tool| tool == "index")),
            "expected no `index` next_action (indexing is automatic); got next_actions={next_actions:?}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn doctor_full_includes_observability_metrics() {
        let _env = ENV_MUTEX.lock().expect("ENV_MUTEX");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");
        std::fs::write(root.join("src/lib.rs"), "pub fn alpha() {}\n").expect("write lib.rs");

        let service = ContextFinderService::new();
        let request = DoctorRequest {
            path: Some(root.to_string_lossy().to_string()),
            response_mode: Some(ResponseMode::Full),
        };

        let output = doctor(&service, request).await.expect("doctor");
        let structured = output
            .structured_content
            .as_ref()
            .expect("structured content");

        let obs = structured.get("observability").unwrap_or_else(|| {
            panic!("observability field present in full mode (structured={structured:?})")
        });
        let indexing = obs.get("indexing").expect("indexing observability");
        assert!(
            indexing
                .get("concurrency_limit")
                .and_then(|v| v.as_u64())
                .is_some_and(|v| v >= 1),
            "expected concurrency_limit >= 1"
        );
        assert!(
            indexing
                .get("concurrency_in_flight")
                .and_then(|v| v.as_u64())
                .is_some(),
            "expected concurrency_in_flight integer"
        );
        assert!(
            indexing
                .get("concurrency_waiters")
                .and_then(|v| v.as_u64())
                .is_some(),
            "expected concurrency_waiters integer"
        );
        assert!(
            indexing
                .get("write_lock_wait_ms_last")
                .and_then(|v| v.as_u64())
                .is_some(),
            "expected write_lock_wait_ms_last integer"
        );
        assert!(
            indexing
                .get("write_lock_wait_ms_max")
                .and_then(|v| v.as_u64())
                .is_some(),
            "expected write_lock_wait_ms_max integer"
        );

        let warm = obs
            .get("warm_indexers")
            .expect("warm_indexers observability");
        assert!(
            warm.get("workers").and_then(|v| v.as_u64()).is_some(),
            "expected warm_indexers.workers integer"
        );
        assert!(
            warm.get("starting").and_then(|v| v.as_u64()).is_some(),
            "expected warm_indexers.starting integer"
        );
        assert!(
            warm.get("lru").and_then(|v| v.as_u64()).is_some(),
            "expected warm_indexers.lru integer"
        );
    }

    #[tokio::test]
    async fn doctor_facts_suppresses_observability_metrics() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");
        std::fs::write(root.join("src/lib.rs"), "pub fn alpha() {}\n").expect("write lib.rs");

        let service = ContextFinderService::new();
        let request = DoctorRequest {
            path: Some(root.to_string_lossy().to_string()),
            response_mode: Some(ResponseMode::Facts),
        };

        let output = doctor(&service, request).await.expect("doctor");
        let structured = output
            .structured_content
            .as_ref()
            .expect("structured content");
        assert!(
            structured.get("observability").is_none(),
            "expected observability to be absent in facts mode"
        );
    }
}
