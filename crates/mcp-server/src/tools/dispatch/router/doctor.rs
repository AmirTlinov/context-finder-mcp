use super::super::{
    load_corpus_chunk_ids, load_index_chunk_ids, load_model_statuses, runtime_env,
    sample_file_paths, CallToolResult, Content, ContextFinderService, DoctorEnvResult,
    DoctorIndexDrift, DoctorProjectResult, DoctorRequest, DoctorResult, McpError,
};
use context_vector_store::corpus_path_for_project_root;
use std::path::Path;

async fn diagnose_project(
    root: &Path,
    issues: &mut Vec<String>,
    hints: &mut Vec<String>,
) -> Option<DoctorProjectResult> {
    let corpus_path = corpus_path_for_project_root(root);
    let has_corpus = corpus_path.exists();

    let indexes_dir = root.join(".context-finder").join("indexes");
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
        hints
            .push("No semantic indexes found for this project. Run the `index` tool first.".into());
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

                    if missing_chunks > 0 || extra_chunks > 0 {
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

                if !drifted_models.is_empty() {
                    issues.push(format!(
                        "Index drift detected vs corpus for models: {}",
                        drifted_models.join(", ")
                    ));
                    hints.push("Run `context-finder index --force --experts` (or the MCP `index` tool) to rebuild semantic indexes to match the current corpus. If you recently changed profiles/models, consider reindexing all models in your roster.".into());
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
        hints.push("Corpus not found for this project; drift detection is unavailable. Run `context-finder index` once to generate corpus + indexes.".into());
    }

    Some(DoctorProjectResult {
        root: root.to_string_lossy().into_owned(),
        corpus_path: corpus_path.to_string_lossy().into_owned(),
        has_corpus,
        indexed_models,
        drift,
    })
}

/// Diagnose model/GPU/index configuration
pub(in crate::tools::dispatch) async fn doctor(
    service: &ContextFinderService,
    request: DoctorRequest,
) -> Result<CallToolResult, McpError> {
    let DoctorRequest { path } = request;
    let model_dir = context_vector_store::model_dir();
    let manifest_path = model_dir.join("manifest.json");

    let (model_manifest_exists, models) = match load_model_statuses(&model_dir).await {
        Ok(result) => result,
        Err(err) => {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Failed to load model manifest {}: {err:#}",
                manifest_path.display()
            ))]));
        }
    };

    let gpu = runtime_env::diagnose_gpu_env();
    let cuda_disabled = runtime_env::is_cuda_disabled();
    let allow_cpu_fallback = std::env::var("CONTEXT_FINDER_ALLOW_CPU")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let mut issues: Vec<String> = Vec::new();
    let mut hints: Vec<String> = Vec::new();

    if !cuda_disabled && (!gpu.provider_present || !gpu.cublas_present) {
        issues.push("CUDA libraries are not fully configured (provider/cublas missing).".into());
        hints.push("Run `bash scripts/setup_cuda_deps.sh` in the Context Finder repo, or set ORT_LIB_LOCATION/LD_LIBRARY_PATH to directories containing libonnxruntime_providers_cuda.so and libcublasLt.so.*. If you want CPU fallback, set CONTEXT_FINDER_ALLOW_CPU=1.".into());
    }

    if !model_manifest_exists {
        issues.push(format!(
            "Model manifest not found at {}",
            manifest_path.display()
        ));
        hints.push("Run `context-finder install-models` (or set CONTEXT_FINDER_MODEL_DIR to a directory containing models/manifest.json).".into());
    } else if models.iter().any(|m| !m.installed) {
        hints.push("Some models are missing assets. Run `context-finder install-models` to download them into the model directory.".into());
    }

    let root = match service.resolve_root(path.as_deref()).await {
        Ok((root, _)) => root,
        Err(message) => return Ok(CallToolResult::error(vec![Content::text(message)])),
    };
    let project = diagnose_project(&root, &mut issues, &mut hints).await;

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
        meta: None,
    };
    result.meta = Some(service.tool_meta(&root).await);

    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&result).unwrap_or_default(),
    )]))
}
