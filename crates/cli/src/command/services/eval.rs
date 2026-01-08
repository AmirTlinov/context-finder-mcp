use super::search::format_basic_output;
use crate::command::context::{
    ensure_index_exists, index_path, index_path_for_model, CommandContext,
};
use crate::command::domain::{
    parse_payload, CommandOutcome, EvalCacheMode, EvalCaseResult, EvalCompareCase,
    EvalCompareOutput, EvalComparePayload, EvalCompareSummary, EvalDatasetMeta, EvalHit,
    EvalOutput, EvalPayload, EvalRun, EvalRunSummary, EvalSummary, SearchOutput,
};
use anyhow::{anyhow, Context as AnyhowContext, Result};
use context_search::{MultiModelHybridSearch, SearchProfile};
use context_vector_store::{
    context_dir_for_project_root, corpus_path_for_project_root, current_model_id, ChunkCorpus,
    QueryKind, VectorIndex, LEGACY_CONTEXT_DIR_NAME,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct EvalService;

impl EvalService {
    pub async fn run(&self, payload: Value, ctx: &CommandContext) -> Result<CommandOutcome> {
        let payload: EvalPayload = parse_payload(payload)?;
        let project_ctx = ctx.resolve_project(payload.path).await?;

        let dataset = load_dataset(&payload.dataset).await?;
        let limit = payload
            .limit
            .unwrap_or(crate::command::domain::DEFAULT_LIMIT)
            .max(1);
        let cache_mode = payload.cache_mode.unwrap_or(EvalCacheMode::Warm);
        let models_filter = normalize_models_filter(payload.models);

        let profiles: Vec<(String, SearchProfile)> = if payload.profiles.is_empty() {
            vec![(
                project_ctx.profile_name.clone(),
                project_ctx.profile.clone(),
            )]
        } else {
            payload
                .profiles
                .iter()
                .map(|name| {
                    load_profile(&project_ctx.root, name).map(|profile| (name.clone(), profile))
                })
                .collect::<Result<Vec<_>>>()?
        };

        let mut runs = Vec::with_capacity(profiles.len());
        for (profile_name, profile) in profiles {
            runs.push(
                evaluate_run(
                    &project_ctx.root,
                    &profile_name,
                    &profile,
                    &dataset,
                    limit,
                    &models_filter,
                    cache_mode,
                )
                .await?,
            );
        }

        CommandOutcome::from_value(EvalOutput {
            dataset: EvalDatasetMeta {
                schema_version: dataset.schema_version,
                name: dataset.name.clone(),
                cases: dataset.cases.len(),
            },
            runs,
        })
    }

    pub async fn compare(&self, payload: Value, ctx: &CommandContext) -> Result<CommandOutcome> {
        let payload: EvalComparePayload = parse_payload(payload)?;
        let project_ctx = ctx.resolve_project(payload.path).await?;

        let dataset = load_dataset(&payload.dataset).await?;
        let limit = payload
            .limit
            .unwrap_or(crate::command::domain::DEFAULT_LIMIT)
            .max(1);
        let cache_mode = payload.cache_mode.unwrap_or(EvalCacheMode::Warm);

        let a_profile_name = payload.a.profile.clone();
        let a_profile = load_profile(&project_ctx.root, &a_profile_name)?;
        let a_models = normalize_models_filter(payload.a.models);

        let b_profile_name = payload.b.profile.clone();
        let b_profile = load_profile(&project_ctx.root, &b_profile_name)?;
        let b_models = normalize_models_filter(payload.b.models);

        let run_a = evaluate_run(
            &project_ctx.root,
            &a_profile_name,
            &a_profile,
            &dataset,
            limit,
            &a_models,
            cache_mode,
        )
        .await?;
        let run_b = evaluate_run(
            &project_ctx.root,
            &b_profile_name,
            &b_profile,
            &dataset,
            limit,
            &b_models,
            cache_mode,
        )
        .await?;

        let (summary, cases) = compare_runs(&run_a, &run_b)?;

        CommandOutcome::from_value(EvalCompareOutput {
            dataset: EvalDatasetMeta {
                schema_version: dataset.schema_version,
                name: dataset.name.clone(),
                cases: dataset.cases.len(),
            },
            cache_mode,
            a: run_summary(&run_a),
            b: run_summary(&run_b),
            summary,
            cases,
        })
    }
}

#[derive(Debug, Deserialize)]
struct EvalDatasetFile {
    schema_version: u32,
    #[serde(default)]
    name: Option<String>,
    cases: Vec<EvalDatasetCase>,
}

#[derive(Debug, Deserialize)]
struct EvalDatasetCase {
    id: String,
    query: String,
    #[serde(default)]
    expected_paths: Vec<String>,
    #[serde(default)]
    expected_symbols: Vec<String>,
    #[serde(default)]
    intent: Option<String>,
}

impl EvalDatasetFile {
    fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            anyhow::bail!(
                "Unsupported eval dataset schema_version {} (expected 1)",
                self.schema_version
            );
        }
        if self.cases.is_empty() {
            anyhow::bail!("Eval dataset must contain at least one case");
        }
        for case in &self.cases {
            if case.id.trim().is_empty() {
                anyhow::bail!("Eval dataset case id must not be empty");
            }
            if case.query.trim().is_empty() {
                anyhow::bail!("Eval dataset case '{}' query must not be empty", case.id);
            }
            if case
                .expected_paths
                .iter()
                .all(|path| path.trim().is_empty())
            {
                anyhow::bail!(
                    "Eval dataset case '{}' expected_paths must not be empty",
                    case.id
                );
            }
        }
        Ok(())
    }
}

async fn load_dataset(path: &Path) -> Result<EvalDatasetFile> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("Failed to read eval dataset {}", path.display()))?;
    let dataset: EvalDatasetFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("Eval dataset {} is not valid JSON", path.display()))?;
    dataset.validate()?;
    Ok(dataset)
}

fn profile_candidates(root: &Path, profile: &str) -> Vec<PathBuf> {
    let mut dirs = vec![context_dir_for_project_root(root)];
    let legacy_dir = root.join(LEGACY_CONTEXT_DIR_NAME);
    if legacy_dir != dirs[0] {
        dirs.push(legacy_dir);
    }

    let mut candidates = Vec::new();
    for dir in dirs {
        let base = dir.join("profiles").join(profile);
        if base.extension().is_none() {
            candidates.push(base.with_extension("json"));
            candidates.push(base.with_extension("toml"));
        } else {
            candidates.push(base);
        }
    }
    candidates
}

fn load_profile(root: &Path, profile_name: &str) -> Result<SearchProfile> {
    for candidate in profile_candidates(root, profile_name) {
        if candidate.exists() {
            let bytes = std::fs::read(&candidate)
                .with_context(|| format!("Failed to read profile {}", candidate.display()))?;
            let base = if profile_name == "general" {
                None
            } else {
                Some("general")
            };
            return SearchProfile::from_bytes(profile_name, &bytes, base).with_context(|| {
                format!(
                    "Failed to parse profile {} as JSON/TOML",
                    candidate.display()
                )
            });
        }
    }

    if let Some(profile) = SearchProfile::builtin(profile_name) {
        return Ok(profile);
    }

    anyhow::bail!(
        "Profile '{}' not found. Expected .context/profiles/{}.json|toml",
        profile_name,
        profile_name
    )
}

async fn load_chunk_corpus(root: &Path) -> Result<Option<ChunkCorpus>> {
    let path = corpus_path_for_project_root(root);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(ChunkCorpus::load(&path).await?))
}

fn semantic_model_roster(profile: &SearchProfile) -> Vec<String> {
    let experts = profile.experts();
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for kind in [
        QueryKind::Identifier,
        QueryKind::Path,
        QueryKind::Conceptual,
    ] {
        for model_id in experts.semantic_models(kind) {
            if seen.insert(model_id.clone()) {
                out.push(model_id.clone());
            }
        }
    }

    out
}

fn normalize_models_filter(models: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for model in models {
        let trimmed = model.trim();
        if trimmed.is_empty() {
            continue;
        }
        let normalized = trimmed.to_string();
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    out
}

async fn load_semantic_indexes_for_models(
    root: &Path,
    profile: &SearchProfile,
    models_filter: &[String],
) -> Result<Vec<(String, VectorIndex)>> {
    let store_path = index_path(root);
    ensure_index_exists(&store_path)?;

    let requested = if models_filter.is_empty() {
        let default_model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
        let mut requested: Vec<String> = Vec::new();
        requested.push(default_model_id.clone());
        requested.extend(semantic_model_roster(profile));
        requested
    } else {
        models_filter.to_vec()
    };

    let mut requested = normalize_models_filter(requested);
    if requested.is_empty() {
        return Err(anyhow!("No embedding models requested for eval"));
    }

    let mut sources = Vec::new();
    let mut missing = Vec::new();
    for model_id in requested.drain(..) {
        let path = index_path_for_model(root, &model_id);
        if !path.exists() {
            missing.push(model_id);
            continue;
        }
        let index = VectorIndex::load(&path)
            .await
            .with_context(|| format!("Failed to load index {}", path.display()))?;
        sources.push((model_id, index));
    }

    if !models_filter.is_empty() && !missing.is_empty() {
        anyhow::bail!(
            "Missing requested indices: {}. Run `context index --models ...` first.",
            missing.join(", ")
        );
    }

    if sources.is_empty() {
        anyhow::bail!(
            "No semantic indices available. Expected at least {}",
            store_path.display()
        );
    }

    Ok(sources)
}

async fn evaluate_run(
    root: &Path,
    profile_name: &str,
    profile: &SearchProfile,
    dataset: &EvalDatasetFile,
    limit: usize,
    models_filter: &[String],
    cache_mode: EvalCacheMode,
) -> Result<EvalRun> {
    match cache_mode {
        EvalCacheMode::Warm => {
            evaluate_run_warm(root, profile_name, profile, dataset, limit, models_filter).await
        }
        EvalCacheMode::Cold => {
            evaluate_run_cold(root, profile_name, profile, dataset, limit, models_filter).await
        }
    }
}

async fn evaluate_run_warm(
    root: &Path,
    profile_name: &str,
    profile: &SearchProfile,
    dataset: &EvalDatasetFile,
    limit: usize,
    models_filter: &[String],
) -> Result<EvalRun> {
    let sources = load_semantic_indexes_for_models(root, profile, models_filter).await?;
    let models: Vec<String> = sources.iter().map(|(id, _)| id.clone()).collect();
    let corpus = load_chunk_corpus(root).await?;
    let mut search = if let Some(corpus) = corpus {
        MultiModelHybridSearch::from_env_with_corpus(sources, profile.clone(), corpus)
    } else {
        MultiModelHybridSearch::from_env(sources, profile.clone())
    }
    .context("Failed to create search engine")?;

    let mut case_results = Vec::with_capacity(dataset.cases.len());
    let mut latencies = Vec::with_capacity(dataset.cases.len());
    let mut bytes = Vec::with_capacity(dataset.cases.len());
    let mut mrrs = Vec::with_capacity(dataset.cases.len());
    let mut recalls = Vec::with_capacity(dataset.cases.len());
    let mut overlaps = Vec::with_capacity(dataset.cases.len());

    for case in &dataset.cases {
        let start = Instant::now();
        let results = search
            .search(&case.query, limit)
            .await
            .with_context(|| format!("Eval search failed for case {}", case.id))?;
        let latency_ms = start.elapsed().as_millis() as u64;

        let metrics = score_case(case, &results, limit)?;

        let hits: Vec<EvalHit> = results
            .iter()
            .take(limit)
            .map(|r| EvalHit {
                id: r.id.clone(),
                file: r.chunk.file_path.clone(),
                start_line: r.chunk.start_line,
                end_line: r.chunk.end_line,
                score: r.score,
            })
            .collect();

        let formatted = results
            .into_iter()
            .take(limit)
            .map(format_basic_output)
            .collect::<Vec<_>>();
        let bytes_len = serde_json::to_vec(&SearchOutput {
            query: case.query.clone(),
            results: formatted,
        })?
        .len();

        latencies.push(latency_ms);
        bytes.push(bytes_len);
        mrrs.push(metrics.mrr);
        recalls.push(metrics.recall);
        overlaps.push(metrics.overlap_ratio);

        case_results.push(EvalCaseResult {
            id: case.id.clone(),
            query: case.query.clone(),
            expected_paths: case.expected_paths.clone(),
            expected_symbols: case.expected_symbols.clone(),
            intent: case.intent.clone(),
            mrr: metrics.mrr,
            recall: metrics.recall,
            overlap_ratio: metrics.overlap_ratio,
            first_rank: metrics.first_rank,
            latency_ms,
            bytes: bytes_len,
            hits,
        });
    }

    Ok(EvalRun {
        profile: profile_name.to_string(),
        models,
        limit,
        cache_mode: EvalCacheMode::Warm,
        summary: EvalSummary {
            mean_mrr: mean_f64(&mrrs),
            mean_recall: mean_f64(&recalls),
            mean_overlap_ratio: mean_f64(&overlaps),
            mean_latency_ms: mean_u64(&latencies),
            p50_latency_ms: percentile_u64(&mut latencies, 0.50),
            p95_latency_ms: percentile_u64(&mut latencies, 0.95),
            mean_bytes: mean_usize(&bytes),
        },
        cases: case_results,
    })
}

async fn evaluate_run_cold(
    root: &Path,
    profile_name: &str,
    profile: &SearchProfile,
    dataset: &EvalDatasetFile,
    limit: usize,
    models_filter: &[String],
) -> Result<EvalRun> {
    let corpus_base = load_chunk_corpus(root).await?;

    // Ensure indices exist and capture the actual list of loaded models.
    let sources = load_semantic_indexes_for_models(root, profile, models_filter).await?;
    let models: Vec<String> = sources.iter().map(|(id, _)| id.clone()).collect();
    drop(sources);

    let mut case_results = Vec::with_capacity(dataset.cases.len());
    let mut latencies = Vec::with_capacity(dataset.cases.len());
    let mut bytes = Vec::with_capacity(dataset.cases.len());
    let mut mrrs = Vec::with_capacity(dataset.cases.len());
    let mut recalls = Vec::with_capacity(dataset.cases.len());
    let mut overlaps = Vec::with_capacity(dataset.cases.len());

    for case in &dataset.cases {
        let start = Instant::now();
        let sources = load_semantic_indexes_for_models(root, profile, models_filter).await?;
        let mut search = if let Some(corpus) = corpus_base.as_ref() {
            MultiModelHybridSearch::from_env_with_corpus(sources, profile.clone(), corpus.clone())
        } else {
            MultiModelHybridSearch::from_env(sources, profile.clone())
        }
        .context("Failed to create search engine")?;

        let results = search
            .search(&case.query, limit)
            .await
            .with_context(|| format!("Eval search failed for case {}", case.id))?;
        let latency_ms = start.elapsed().as_millis() as u64;

        let metrics = score_case(case, &results, limit)?;

        let hits: Vec<EvalHit> = results
            .iter()
            .take(limit)
            .map(|r| EvalHit {
                id: r.id.clone(),
                file: r.chunk.file_path.clone(),
                start_line: r.chunk.start_line,
                end_line: r.chunk.end_line,
                score: r.score,
            })
            .collect();

        let formatted = results
            .into_iter()
            .take(limit)
            .map(format_basic_output)
            .collect::<Vec<_>>();
        let bytes_len = serde_json::to_vec(&SearchOutput {
            query: case.query.clone(),
            results: formatted,
        })?
        .len();

        latencies.push(latency_ms);
        bytes.push(bytes_len);
        mrrs.push(metrics.mrr);
        recalls.push(metrics.recall);
        overlaps.push(metrics.overlap_ratio);

        case_results.push(EvalCaseResult {
            id: case.id.clone(),
            query: case.query.clone(),
            expected_paths: case.expected_paths.clone(),
            expected_symbols: case.expected_symbols.clone(),
            intent: case.intent.clone(),
            mrr: metrics.mrr,
            recall: metrics.recall,
            overlap_ratio: metrics.overlap_ratio,
            first_rank: metrics.first_rank,
            latency_ms,
            bytes: bytes_len,
            hits,
        });
    }

    Ok(EvalRun {
        profile: profile_name.to_string(),
        models,
        limit,
        cache_mode: EvalCacheMode::Cold,
        summary: EvalSummary {
            mean_mrr: mean_f64(&mrrs),
            mean_recall: mean_f64(&recalls),
            mean_overlap_ratio: mean_f64(&overlaps),
            mean_latency_ms: mean_u64(&latencies),
            p50_latency_ms: percentile_u64(&mut latencies, 0.50),
            p95_latency_ms: percentile_u64(&mut latencies, 0.95),
            mean_bytes: mean_usize(&bytes),
        },
        cases: case_results,
    })
}

struct CaseMetrics {
    mrr: f64,
    recall: f64,
    overlap_ratio: f64,
    first_rank: Option<usize>,
}

fn score_case(
    case: &EvalDatasetCase,
    results: &[context_vector_store::SearchResult],
    limit: usize,
) -> Result<CaseMetrics> {
    let expected: HashSet<&str> = case
        .expected_paths
        .iter()
        .map(|path| path.trim())
        .filter(|path| !path.is_empty())
        .collect();
    if expected.is_empty() {
        return Err(anyhow!("Eval case '{}' has empty expected_paths", case.id));
    }

    let mut found: HashSet<&str> = HashSet::new();
    let mut predicted: HashSet<&str> = HashSet::new();
    let mut first_rank: Option<usize> = None;
    for (idx, hit) in results.iter().take(limit).enumerate() {
        let file = hit.chunk.file_path.as_str();
        predicted.insert(file);
        if expected.contains(file) {
            found.insert(file);
            if first_rank.is_none() {
                first_rank = Some(idx + 1);
            }
        }
    }

    let recall = found.len() as f64 / expected.len() as f64;
    let union_size = expected.union(&predicted).count().max(1);
    let overlap_ratio = found.len() as f64 / union_size as f64;
    let mrr = first_rank.map(|rank| 1.0 / (rank as f64)).unwrap_or(0.0);

    Ok(CaseMetrics {
        mrr,
        recall,
        overlap_ratio,
        first_rank,
    })
}

fn run_summary(run: &EvalRun) -> EvalRunSummary {
    EvalRunSummary {
        profile: run.profile.clone(),
        models: run.models.clone(),
        limit: run.limit,
        cache_mode: run.cache_mode,
        summary: EvalSummary {
            mean_mrr: run.summary.mean_mrr,
            mean_recall: run.summary.mean_recall,
            mean_overlap_ratio: run.summary.mean_overlap_ratio,
            mean_latency_ms: run.summary.mean_latency_ms,
            p50_latency_ms: run.summary.p50_latency_ms,
            p95_latency_ms: run.summary.p95_latency_ms,
            mean_bytes: run.summary.mean_bytes,
        },
    }
}

fn compare_runs(
    run_a: &EvalRun,
    run_b: &EvalRun,
) -> Result<(EvalCompareSummary, Vec<EvalCompareCase>)> {
    if run_a.cases.len() != run_b.cases.len() {
        anyhow::bail!("Eval compare requires matching datasets for A and B");
    }

    let mut a_wins = 0usize;
    let mut b_wins = 0usize;
    let mut ties = 0usize;

    let mut cases = Vec::with_capacity(run_a.cases.len());
    for (a, b) in run_a.cases.iter().zip(run_b.cases.iter()) {
        if a.id != b.id {
            anyhow::bail!(
                "Eval compare requires matching case ids ({} vs {})",
                a.id,
                b.id
            );
        }

        match b.mrr.partial_cmp(&a.mrr) {
            Some(std::cmp::Ordering::Greater) => b_wins += 1,
            Some(std::cmp::Ordering::Less) => a_wins += 1,
            _ => match b.recall.partial_cmp(&a.recall) {
                Some(std::cmp::Ordering::Greater) => b_wins += 1,
                Some(std::cmp::Ordering::Less) => a_wins += 1,
                _ => ties += 1,
            },
        }

        cases.push(EvalCompareCase {
            id: a.id.clone(),
            query: a.query.clone(),
            expected_paths: a.expected_paths.clone(),
            a_mrr: a.mrr,
            b_mrr: b.mrr,
            delta_mrr: b.mrr - a.mrr,
            a_recall: a.recall,
            b_recall: b.recall,
            delta_recall: b.recall - a.recall,
            a_overlap_ratio: a.overlap_ratio,
            b_overlap_ratio: b.overlap_ratio,
            delta_overlap_ratio: b.overlap_ratio - a.overlap_ratio,
            a_latency_ms: a.latency_ms,
            b_latency_ms: b.latency_ms,
            delta_latency_ms: b.latency_ms as i64 - a.latency_ms as i64,
            a_bytes: a.bytes,
            b_bytes: b.bytes,
            delta_bytes: b.bytes as i64 - a.bytes as i64,
            a_first_rank: a.first_rank,
            b_first_rank: b.first_rank,
        });
    }

    let summary = EvalCompareSummary {
        delta_mean_mrr: run_b.summary.mean_mrr - run_a.summary.mean_mrr,
        delta_mean_recall: run_b.summary.mean_recall - run_a.summary.mean_recall,
        delta_mean_overlap_ratio: run_b.summary.mean_overlap_ratio
            - run_a.summary.mean_overlap_ratio,
        delta_mean_latency_ms: run_b.summary.mean_latency_ms - run_a.summary.mean_latency_ms,
        delta_p95_latency_ms: run_b.summary.p95_latency_ms as i64
            - run_a.summary.p95_latency_ms as i64,
        delta_mean_bytes: run_b.summary.mean_bytes - run_a.summary.mean_bytes,
        a_wins,
        b_wins,
        ties,
    };

    Ok((summary, cases))
}

fn mean_f64(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

fn mean_u64(values: &[u64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let sum: u64 = values.iter().sum();
    sum as f64 / values.len() as f64
}

fn mean_usize(values: &[usize]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let sum: usize = values.iter().sum();
    sum as f64 / values.len() as f64
}

fn percentile_u64(values: &mut [u64], pct: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let n = values.len();
    let rank = ((pct.clamp(0.0, 1.0) * n as f64).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1);
    values[rank]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_validation_rejects_empty_cases() {
        let dataset = EvalDatasetFile {
            schema_version: 1,
            name: None,
            cases: Vec::new(),
        };
        assert!(dataset.validate().is_err());
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let mut values = vec![10, 20, 30, 40, 50];
        assert_eq!(percentile_u64(&mut values, 0.50), 30);
        let mut values = vec![10, 20, 30, 40, 50];
        assert_eq!(percentile_u64(&mut values, 0.95), 50);
    }
}
