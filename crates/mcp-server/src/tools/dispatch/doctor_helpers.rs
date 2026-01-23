use crate::tools::schemas::doctor::DoctorModelStatus;
use anyhow::{Context as AnyhowContext, Result};
use context_vector_store::ChunkCorpus;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Deserialize)]
struct ModelManifestFile {
    models: Vec<ModelManifestModel>,
}

#[derive(Debug, Deserialize)]
struct ModelManifestModel {
    id: String,
    assets: Vec<ModelManifestAsset>,
}

#[derive(Debug, Deserialize)]
struct ModelManifestAsset {
    path: String,
}

fn validate_relative_model_asset_path(path: &Path) -> Result<()> {
    let mut has_component = false;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                anyhow::bail!("asset path must be relative");
            }
            Component::ParentDir => {
                anyhow::bail!("asset path must not contain '..'");
            }
            Component::CurDir => {}
            Component::Normal(_) => {
                has_component = true;
            }
        }
    }
    if !has_component {
        anyhow::bail!("asset path is empty");
    }
    Ok(())
}

fn safe_join_model_asset_path(model_dir: &Path, asset_path: &str) -> Result<PathBuf> {
    let rel = Path::new(asset_path);
    validate_relative_model_asset_path(rel)
        .with_context(|| format!("Invalid model asset path '{asset_path}'"))?;
    Ok(model_dir.join(rel))
}

#[cfg(test)]
mod model_asset_path_tests {
    use super::*;

    #[test]
    fn safe_join_rejects_traversal_and_absolute_paths() {
        let base = Path::new("models");
        assert!(safe_join_model_asset_path(base, "../escape").is_err());
        assert!(safe_join_model_asset_path(base, "m1/../escape").is_err());
        assert!(safe_join_model_asset_path(base, "").is_err());

        #[cfg(unix)]
        assert!(safe_join_model_asset_path(base, "/etc/passwd").is_err());
    }

    #[test]
    fn safe_join_accepts_normal_relative_paths() {
        let base = Path::new("models");
        let path = safe_join_model_asset_path(base, "m1/model.onnx").expect("valid path");
        assert!(path.starts_with(base));
    }
}

#[derive(Debug, Deserialize)]
struct IndexIdMapOnly {
    #[serde(default)]
    schema_version: Option<u32>,
    #[serde(default)]
    id_map: HashMap<usize, String>,
}

pub(super) async fn load_model_statuses(
    model_dir: &Path,
) -> Result<(bool, Vec<DoctorModelStatus>)> {
    let manifest_path = model_dir.join("manifest.json");
    if !manifest_path.exists() {
        return Ok((false, Vec::new()));
    }

    let bytes = tokio::fs::read(&manifest_path)
        .await
        .with_context(|| format!("Failed to read model manifest {}", manifest_path.display()))?;
    let parsed: ModelManifestFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("Failed to parse model manifest {}", manifest_path.display()))?;

    let mut statuses = Vec::new();
    for model in parsed.models {
        let mut missing = Vec::new();
        for asset in model.assets {
            let full = match safe_join_model_asset_path(model_dir, &asset.path) {
                Ok(path) => path,
                Err(err) => {
                    missing.push(format!("invalid_path: {} ({err})", asset.path));
                    continue;
                }
            };
            if !full.exists() {
                missing.push(asset.path);
            }
        }
        let installed = missing.is_empty();
        statuses.push(DoctorModelStatus {
            id: model.id,
            installed,
            missing_assets: missing,
        });
    }
    Ok((true, statuses))
}

pub(super) async fn load_corpus_chunk_ids(corpus_path: &Path) -> Result<HashSet<String>> {
    let corpus = ChunkCorpus::load(corpus_path).await?;
    let mut ids = HashSet::new();
    for chunks in corpus.files().values() {
        for chunk in chunks {
            ids.insert(format!(
                "{}:{}:{}",
                chunk.file_path, chunk.start_line, chunk.end_line
            ));
        }
    }
    Ok(ids)
}

pub(super) async fn load_index_chunk_ids(index_path: &Path) -> Result<HashSet<String>> {
    let bytes = tokio::fs::read(index_path)
        .await
        .with_context(|| format!("Failed to read index {}", index_path.display()))?;
    let parsed: IndexIdMapOnly = serde_json::from_slice(&bytes)
        .with_context(|| format!("Failed to parse index {}", index_path.display()))?;
    // schema_version is tracked for diagnostics, but chunk id extraction relies on id_map values.
    let _ = parsed.schema_version.unwrap_or(1);
    Ok(parsed.id_map.into_values().collect())
}

fn chunk_id_file_path(chunk_id: &str) -> Option<String> {
    let mut parts = chunk_id.rsplitn(3, ':');
    let _end = parts.next()?;
    let _start = parts.next()?;
    Some(parts.next()?.to_string())
}

pub(super) fn sample_file_paths<'a, I>(chunk_ids: I, limit: usize) -> Vec<String>
where
    I: Iterator<Item = &'a String>,
{
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for id in chunk_ids {
        if out.len() >= limit {
            break;
        }
        let Some(file) = chunk_id_file_path(id) else {
            continue;
        };
        if seen.insert(file.clone()) {
            out.push(file);
        }
    }
    out
}
