use anyhow::{Context as AnyhowContext, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Deserialize)]
pub struct ModelsManifest {
    pub schema_version: u32,
    pub models: Vec<ModelSpec>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ModelSpec {
    pub id: String,
    pub description: Option<String>,
    pub dimension: usize,
    pub max_length: usize,
    pub max_batch: usize,
    pub assets: Vec<ModelAsset>,
}

#[derive(Debug, Deserialize)]
pub struct ModelAsset {
    pub path: String,
    pub sha256: String,
    pub source: AssetSource,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssetSource {
    Huggingface {
        repo: String,
        revision: String,
        filename: String,
    },
    Url {
        url: String,
    },
}

impl AssetSource {
    fn url(&self) -> String {
        match self {
            Self::Url { url } => url.clone(),
            Self::Huggingface {
                repo,
                revision,
                filename,
            } => format!("https://huggingface.co/{repo}/resolve/{revision}/{filename}"),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct InstallModelsReport {
    pub model_dir: String,
    pub selected_models: Vec<String>,
    pub skipped: Vec<String>,
    pub downloaded: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub model_dir: String,
    pub profile: String,
    pub embedding_mode: String,
    pub embedding_model: String,
    pub allow_cpu_fallback: bool,
    pub gpu_ok: bool,
    pub gpu_error: Option<String>,
    pub manifest_ok: bool,
    pub manifest_error: Option<String>,
    pub models: Vec<ModelDoctorItem>,
}

#[derive(Debug, Serialize)]
pub struct ModelDoctorItem {
    pub id: String,
    pub ok: bool,
    pub missing_assets: Vec<String>,
    pub bad_sha256: Vec<BadSha>,
}

#[derive(Debug, Serialize)]
pub struct BadSha {
    pub path: String,
    pub expected: String,
    pub actual: String,
}

pub fn resolve_model_dir() -> PathBuf {
    if let Ok(path) = std::env::var("CONTEXT_MODEL_DIR") {
        return PathBuf::from(path);
    }
    PathBuf::from("models")
}

pub fn load_manifest(model_dir: &Path) -> Result<ModelsManifest> {
    let manifest_path = model_dir.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("Failed to read {}", manifest_path.display()))?;
    let manifest: ModelsManifest = serde_json::from_str(&raw)
        .with_context(|| format!("Invalid {}", manifest_path.display()))?;
    if manifest.schema_version != 1 {
        anyhow::bail!(
            "Unsupported models manifest schema_version {} (expected 1)",
            manifest.schema_version
        );
    }
    Ok(manifest)
}

fn validate_relative_asset_path(path: &Path) -> Result<()> {
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

pub(crate) fn safe_join_asset_path(model_dir: &Path, asset_path: &str) -> Result<PathBuf> {
    let rel = Path::new(asset_path);
    validate_relative_asset_path(rel)
        .with_context(|| format!("Invalid model asset path '{asset_path}'"))?;
    Ok(model_dir.join(rel))
}

pub async fn install_models(
    model_dir: &Path,
    selected: &[String],
    force: bool,
    dry_run: bool,
) -> Result<InstallModelsReport> {
    let manifest = load_manifest(model_dir)?;

    let selected_set: Option<HashSet<&str>> = if selected.is_empty() {
        None
    } else {
        Some(selected.iter().map(String::as_str).collect())
    };

    let mut selected_models = Vec::new();
    let mut skipped = Vec::new();
    let mut downloaded = Vec::new();

    let client = Client::builder()
        .build()
        .context("Failed to build HTTP client")?;

    for model in &manifest.models {
        if let Some(set) = &selected_set {
            if !set.contains(model.id.as_str()) {
                continue;
            }
        }
        selected_models.push(model.id.clone());

        for asset in &model.assets {
            let local_path = safe_join_asset_path(model_dir, &asset.path)
                .with_context(|| format!("Invalid asset path for model '{}'", model.id))?;
            let expected = asset.sha256.trim().to_ascii_lowercase();

            if local_path.exists() && !force && !expected.is_empty() {
                let actual = sha256_file(&local_path)
                    .with_context(|| format!("Failed to hash {}", local_path.display()))?;
                if actual == expected {
                    skipped.push(asset.path.clone());
                    continue;
                }
            }

            if dry_run {
                downloaded.push(asset.path.clone());
                continue;
            }

            if let Some(parent) = local_path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create directory {}", parent.display()))?;
            }

            let url = asset.source.url();
            let tmp_path = temp_path_for(&local_path);

            download_with_sha256(&client, &url, &tmp_path, expected.as_str()).await?;

            // Atomic swap: move aside existing file if present.
            if local_path.exists() {
                let backup = backup_path_for(&local_path);
                std::fs::rename(&local_path, &backup).with_context(|| {
                    format!(
                        "Failed to move existing {} to {}",
                        local_path.display(),
                        backup.display()
                    )
                })?;
            }
            std::fs::rename(&tmp_path, &local_path).with_context(|| {
                format!(
                    "Failed to move {} to {}",
                    tmp_path.display(),
                    local_path.display()
                )
            })?;

            downloaded.push(asset.path.clone());
        }
    }

    Ok(InstallModelsReport {
        model_dir: model_dir.display().to_string(),
        selected_models,
        skipped,
        downloaded,
    })
}

pub fn doctor(model_dir: &Path) -> DoctorReport {
    let mut report = DoctorReport {
        model_dir: model_dir.display().to_string(),
        profile: std::env::var("CONTEXT_PROFILE").unwrap_or_else(|_| "quality".to_string()),
        embedding_mode: std::env::var("CONTEXT_EMBEDDING_MODE")
            .unwrap_or_else(|_| "fast".to_string()),
        embedding_model: std::env::var("CONTEXT_EMBEDDING_MODEL")
            .unwrap_or_else(|_| "bge-small".to_string()),
        allow_cpu_fallback: std::env::var("CONTEXT_ALLOW_CPU")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
        gpu_ok: false,
        gpu_error: None,
        manifest_ok: false,
        manifest_error: None,
        models: Vec::new(),
    };

    let manifest = match load_manifest(model_dir) {
        Ok(m) => m,
        Err(e) => {
            report.manifest_error = Some(format!("{e:#}"));
            return report;
        }
    };
    report.manifest_ok = true;

    for model in &manifest.models {
        let mut item = ModelDoctorItem {
            id: model.id.clone(),
            ok: true,
            missing_assets: Vec::new(),
            bad_sha256: Vec::new(),
        };

        for asset in &model.assets {
            let local = match safe_join_asset_path(model_dir, &asset.path) {
                Ok(path) => path,
                Err(err) => {
                    item.ok = false;
                    item.missing_assets
                        .push(format!("invalid_path: {} ({err})", asset.path));
                    continue;
                }
            };
            if !local.exists() {
                item.ok = false;
                item.missing_assets.push(asset.path.clone());
                continue;
            }

            let expected = asset.sha256.trim().to_ascii_lowercase();
            if expected.is_empty() {
                continue;
            }

            match sha256_file(&local) {
                Ok(actual) => {
                    if actual != expected {
                        item.ok = false;
                        item.bad_sha256.push(BadSha {
                            path: asset.path.clone(),
                            expected,
                            actual,
                        });
                    }
                }
                Err(err) => {
                    item.ok = false;
                    item.bad_sha256.push(BadSha {
                        path: asset.path.clone(),
                        expected,
                        actual: format!("hash_error: {err}"),
                    });
                }
            }
        }

        report.models.push(item);
    }

    // Best-effort GPU/runtime check. This validates CUDA EP availability and that the default
    // embedding model can be loaded under the current process environment.
    match context_vector_store::EmbeddingModel::new() {
        Ok(_) => report.gpu_ok = true,
        Err(e) => report.gpu_error = Some(format!("{e:#}")),
    }

    report
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok(to_lower_hex(&digest))
}

async fn download_with_sha256(
    client: &Client,
    url: &str,
    dest: &Path,
    expected: &str,
) -> Result<()> {
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Download failed: GET {url}"))?
        .error_for_status()
        .with_context(|| format!("Download failed: GET {url}"))?;

    let mut file = std::fs::File::create(dest)
        .with_context(|| format!("Failed to create {}", dest.display()))?;
    let mut hasher = Sha256::new();

    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("Failed while reading HTTP body from {url}"))?
    {
        file.write_all(&chunk)
            .with_context(|| format!("Failed to write {}", dest.display()))?;
        hasher.update(&chunk);
    }
    file.flush()
        .with_context(|| format!("Failed to flush {}", dest.display()))?;

    if !expected.is_empty() {
        let actual = to_lower_hex(&hasher.finalize());
        if actual != expected.to_ascii_lowercase() {
            std::fs::remove_file(dest).ok();
            anyhow::bail!(
                "SHA256 mismatch for {}: expected {} actual {}",
                dest.display(),
                expected,
                actual
            );
        }
    }

    Ok(())
}

fn to_lower_hex(bytes: &[u8]) -> String {
    const LUT: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(LUT[(byte >> 4) as usize] as char);
        out.push(LUT[(byte & 0x0f) as usize] as char);
    }
    out
}

fn temp_path_for(path: &Path) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    PathBuf::from(format!("{}.{}.download", path.display(), ts))
}

fn backup_path_for(path: &Path) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    PathBuf::from(format!("{}.{}.bak", path.display(), ts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_asset_path_rejects_traversal_and_absolute_paths() {
        let base = Path::new("models");

        assert!(safe_join_asset_path(base, "../escape").is_err());
        assert!(safe_join_asset_path(base, "bge-small/../escape").is_err());
        assert!(safe_join_asset_path(base, "").is_err());

        #[cfg(unix)]
        assert!(safe_join_asset_path(base, "/etc/passwd").is_err());
    }

    #[test]
    fn safe_join_asset_path_accepts_normal_relative_paths() {
        let base = Path::new("models");
        let path = safe_join_asset_path(base, "bge-small/model.onnx").expect("valid path");
        assert!(path.starts_with(base));
    }
}
