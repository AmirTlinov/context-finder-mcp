use crate::command::domain::{
    config_bool_path, config_string_path, merge_configs, normalize_config, Hint, HintKind,
};
use anyhow::{anyhow, Context as AnyhowContext, Result};
use context_search::SearchProfile;
use context_vector_store::{
    context_dir_for_project_root, current_model_id, LEGACY_CONTEXT_DIR_NAME,
};
use serde_json::Value;
use std::env;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::sync::Mutex;

pub struct CommandContext {
    request_config: Option<Value>,
    request_options: Option<crate::command::domain::RequestOptions>,
    profile_name: String,
    resolved: Mutex<Option<ProjectContext>>,
}

impl CommandContext {
    pub fn new(
        config: Option<Value>,
        options: Option<crate::command::domain::RequestOptions>,
    ) -> Self {
        Self {
            request_config: normalize_config(config),
            request_options: options,
            profile_name: env::var("CONTEXT_PROFILE")
                .or_else(|_| env::var("CONTEXT_FINDER_PROFILE"))
                .unwrap_or_else(|_| "quality".to_string()),
            resolved: Mutex::new(None),
        }
    }

    pub fn request_options(&self) -> crate::command::domain::RequestOptions {
        self.request_options.clone().unwrap_or_default()
    }

    pub async fn resolve_project(&self, provided: Option<PathBuf>) -> Result<ProjectContext> {
        let root = resolve_project_root(provided)?;

        if let Some(cached) = self.resolved.lock().await.as_ref() {
            if cached.root == root {
                return Ok(cached.clone());
            }
        }

        let (file_config, config_path, mut hints) = self.load_file_config(&root).await?;

        let merged = merge_configs(file_config, self.request_config.clone());
        if merged.is_none() {
            hints.push(Hint {
                kind: HintKind::Info,
                text: "Config not found — using defaults. Create .agents/mcp/context/.context/config.json to pin settings."
                    .to_string(),
            });
        }

        apply_env_fallback(
            "CONTEXT_EMBEDDING_MODE",
            "CONTEXT_FINDER_EMBEDDING_MODE",
            &merged,
            &[&["embed_mode"]],
        );
        apply_env_fallback(
            "CONTEXT_EMBEDDING_MODEL",
            "CONTEXT_FINDER_EMBEDDING_MODEL",
            &merged,
            &[&["embedding_model"], &["defaults", "embedding_model"]],
        );
        apply_env_fallback(
            "CONTEXT_MODEL_DIR",
            "CONTEXT_FINDER_MODEL_DIR",
            &merged,
            &[&["model_dir"], &["defaults", "model_dir"]],
        );
        apply_env_fallback(
            "CONTEXT_CUDA_DEVICE",
            "CONTEXT_FINDER_CUDA_DEVICE",
            &merged,
            &[&["cuda_device"], &["defaults", "cuda_device"]],
        );
        apply_env_fallback(
            "CONTEXT_CUDA_MEM_LIMIT_MB",
            "CONTEXT_FINDER_CUDA_MEM_LIMIT_MB",
            &merged,
            &[&["cuda_mem_limit_mb"], &["defaults", "cuda_mem_limit_mb"]],
        );

        let (profile, profile_path, mut profile_hints) = self.load_profile(&root).await?;
        hints.append(&mut profile_hints);

        let resolved = ProjectContext {
            root,
            config: merged,
            config_path,
            profile,
            profile_path,
            profile_name: self.profile_name.clone(),
            hints,
        };

        *self.resolved.lock().await = Some(resolved.clone());
        Ok(resolved)
    }

    async fn load_file_config(
        &self,
        root: &Path,
    ) -> Result<(Option<Value>, Option<String>, Vec<Hint>)> {
        let mut hints = Vec::new();
        let context_dir = context_dir_for_project_root(root);
        let mut candidates = vec![context_dir.join("config.json")];
        let legacy_dir = root.join(LEGACY_CONTEXT_DIR_NAME);
        if legacy_dir != context_dir {
            candidates.push(legacy_dir.join("config.json"));
        }
        let path = match candidates.into_iter().find(|p| p.exists()) {
            Some(path) => path,
            None => {
                return Ok((None, None, hints));
            }
        };
        if !path.exists() {
            return Ok((None, None, hints));
        }

        match fs::read(&path).await {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) => Ok((Some(value), Some(path.display().to_string()), hints)),
                Err(err) => {
                    hints.push(Hint {
                        kind: HintKind::Warn,
                        text: format!("Config parsing failed ({}): ignoring file", path.display()),
                    });
                    log::warn!("Config parse error {}: {err}", path.display());
                    Ok((None, Some(path.display().to_string()), hints))
                }
            },
            Err(err) => Err(anyhow!("Failed to read config {}: {err}", path.display())),
        }
    }

    async fn load_profile(
        &self,
        root: &Path,
    ) -> Result<(SearchProfile, Option<String>, Vec<Hint>)> {
        let mut hints = Vec::new();
        let candidates = profile_candidates(root, &self.profile_name);
        for candidate in &candidates {
            if candidate.exists() {
                let bytes = fs::read(candidate)
                    .await
                    .with_context(|| format!("Failed to read profile {}", candidate.display()))?;
                let base = if self.profile_name == "general" {
                    None
                } else {
                    Some("general")
                };
                let profile = SearchProfile::from_bytes(&self.profile_name, &bytes, base)
                    .with_context(|| {
                        format!(
                            "Failed to parse profile {} as JSON/TOML",
                            candidate.display()
                        )
                    })?;
                return Ok((profile, Some(candidate.display().to_string()), hints));
            }
        }

        if let Some(profile) = SearchProfile::builtin(&self.profile_name) {
            hints.push(Hint {
                kind: HintKind::Info,
                text: format!(
                    "Profile '{}' loaded from built-in bundle (no file in project)",
                    self.profile_name
                ),
            });
            return Ok((profile, None, hints));
        }

        if self.profile_name != "general" {
            return Err(anyhow!(
                "Profile '{}' not found. Checked: {}",
                self.profile_name,
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        let fallback = SearchProfile::general();
        hints.push(Hint {
            kind: HintKind::Info,
            text: "Profile not provided; using built-in general rules".to_string(),
        });
        Ok((fallback, None, hints))
    }
}

#[derive(Clone)]
pub struct ProjectContext {
    pub root: PathBuf,
    pub config: Option<Value>,
    pub config_path: Option<String>,
    pub profile: SearchProfile,
    pub profile_path: Option<String>,
    pub profile_name: String,
    pub hints: Vec<Hint>,
}

fn apply_env_fallback(primary: &str, legacy: &str, config: &Option<Value>, paths: &[&[&str]]) {
    if std::env::var(primary).is_ok() || std::env::var(legacy).is_ok() {
        return;
    }
    for path in paths {
        if let Some(value) = config_string_path(config, path) {
            env::set_var(primary, value);
            break;
        }
    }
}

pub fn index_path(project: &Path) -> PathBuf {
    let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    index_path_for_model(project, &model_id)
}

pub fn index_path_for_model(project: &Path, model_id: &str) -> PathBuf {
    let model_dir = model_id_dir_name(model_id);
    context_dir_for_project_root(project)
        .join("indexes")
        .join(model_dir)
        .join("index.json")
}

pub fn graph_nodes_path(project: &Path) -> PathBuf {
    let model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
    graph_nodes_path_for_model(project, &model_id)
}

pub fn graph_nodes_path_for_model(project: &Path, model_id: &str) -> PathBuf {
    let model_dir = model_id_dir_name(model_id);
    context_dir_for_project_root(project)
        .join("indexes")
        .join(model_dir)
        .join("graph_nodes.json")
}

fn resolve_project_root(provided: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = provided {
        return canonicalize_root(path);
    }

    if let Some(path) = env_root_override() {
        return canonicalize_root(path)
            .with_context(|| "Project path from CONTEXT_ROOT/CONTEXT_PROJECT_ROOT is invalid");
    }

    let cwd = env::current_dir().context("Failed to determine current directory")?;
    let candidate = find_project_root(&cwd).unwrap_or(cwd);
    canonicalize_root(candidate)
}

fn env_root_override() -> Option<PathBuf> {
    for key in [
        "CONTEXT_ROOT",
        "CONTEXT_PROJECT_ROOT",
        "CONTEXT_FINDER_ROOT",
        "CONTEXT_FINDER_PROJECT_ROOT",
    ] {
        if let Ok(value) = env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }
    None
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(PathBuf::from)
}

fn find_project_root(start: &Path) -> Option<PathBuf> {
    if let Some(root) = find_git_root(start) {
        return Some(root);
    }

    const MARKERS: &[&str] = &[
        "AGENTS.md",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "CMakeLists.txt",
        "Makefile",
    ];

    start
        .ancestors()
        .find(|candidate| MARKERS.iter().any(|marker| candidate.join(marker).exists()))
        .map(PathBuf::from)
}

fn canonicalize_root(path: PathBuf) -> Result<PathBuf> {
    if !path.exists() {
        anyhow::bail!("Project path does not exist: {}", path.display());
    }
    path.canonicalize()
        .with_context(|| format!("Failed to canonicalize {}", path.display()))
}

pub fn ensure_index_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!(
            "Index not found at {}. Run 'context index' first.",
            path.display()
        ));
    }
    Ok(())
}

fn model_id_dir_name(model_id: &str) -> String {
    model_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

pub async fn load_store_mtime(path: &Path) -> Result<SystemTime> {
    let metadata = fs::metadata(path)
        .await
        .with_context(|| format!("Failed to read metadata for {}", path.display()))?;
    metadata
        .modified()
        .with_context(|| format!("Failed to read modification time for {}", path.display()))
}

pub fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn graph_language_from_config(config: &Option<Value>) -> Option<String> {
    config_string_path(config, &["graph_language"]).or_else(|| {
        config_bool_path(config, &["graph", "language"]).and(None) // placeholder for forward compatibility
    })
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
