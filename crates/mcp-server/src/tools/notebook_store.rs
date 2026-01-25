use anyhow::{Context as AnyhowContext, Result};
use context_vector_store::{context_dir_for_project_root, CONTEXT_DIR_NAME};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::notebook_types::{AgentNotebook, NotebookAnchor, NotebookRepo, NotebookScope};
use super::secrets::is_potential_secret_path;
use super::util::hex_encode_lower;

const NOTEBOOK_VERSION: u32 = 1;
const NOTEBOOK_FILE_NAME: &str = "notebook_v1.json";
const NOTEBOOK_DIR_NAME: &str = "notebook";
const NOTEBOOK_LOCK_NAME: &str = "notebook.lock";

#[derive(Debug, Clone)]
pub(crate) struct RepoIdentity {
    pub repo_id: String,
    pub repo_kind: String,
}

#[derive(Debug, Clone)]
pub(crate) struct NotebookPaths {
    pub notebook_path: PathBuf,
    pub lock_path: PathBuf,
    pub repo: RepoIdentity,
}

pub(crate) fn resolve_repo_identity(root: &Path) -> RepoIdentity {
    if let Some(common_dir) = git_common_dir(root) {
        let repo_id = sha256_hex(common_dir.to_string_lossy().as_bytes());
        return RepoIdentity {
            repo_id,
            repo_kind: "git".to_string(),
        };
    }
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    RepoIdentity {
        repo_id: sha256_hex(canonical.to_string_lossy().as_bytes()),
        repo_kind: "fs".to_string(),
    }
}

fn git_common_dir(root: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("--git-common-dir")
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(trimmed);
    let joined = if candidate.is_absolute() {
        candidate
    } else {
        root.join(candidate)
    };
    joined.canonicalize().ok()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode_lower(&hasher.finalize())
}

fn home_context_base_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(CONTEXT_DIR_NAME))
}

pub(crate) fn notebook_paths_for_scope(
    root: &Path,
    scope: NotebookScope,
    identity: &RepoIdentity,
) -> Result<NotebookPaths> {
    let (dir, lock_dir) = match scope {
        NotebookScope::Project => {
            let base = context_dir_for_project_root(root).join(NOTEBOOK_DIR_NAME);
            (base.clone(), base)
        }
        NotebookScope::UserRepo => {
            let base = home_context_base_dir().context("home_dir unavailable")?;
            let dir = base
                .join("notebooks")
                .join(safe_dir_component(&identity.repo_id));
            (dir.clone(), dir)
        }
    };

    Ok(NotebookPaths {
        notebook_path: dir.join(NOTEBOOK_FILE_NAME),
        lock_path: lock_dir.join(NOTEBOOK_LOCK_NAME),
        repo: identity.clone(),
    })
}

fn safe_dir_component(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

pub(crate) struct NotebookLock {
    #[allow(dead_code)]
    file: File,
}

impl Drop for NotebookLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub(crate) fn acquire_notebook_lock(lock_path: &Path) -> Result<NotebookLock> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| "create notebook lock dir")?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("open notebook lock {}", lock_path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("lock notebook {}", lock_path.display()))?;
    Ok(NotebookLock { file })
}

pub(crate) fn load_or_init_notebook(_root: &Path, paths: &NotebookPaths) -> Result<AgentNotebook> {
    if paths.notebook_path.exists() {
        let raw = std::fs::read_to_string(&paths.notebook_path)
            .with_context(|| format!("read notebook {}", paths.notebook_path.display()))?;
        let mut notebook: AgentNotebook = serde_json::from_str(&raw)
            .with_context(|| format!("parse notebook json {}", paths.notebook_path.display()))?;
        // Ensure repo identity stays consistent even if an older notebook was copied.
        notebook.repo.repo_id = paths.repo.repo_id.clone();
        notebook.repo.repo_kind = paths.repo.repo_kind.clone();
        notebook.version = NOTEBOOK_VERSION;
        return Ok(notebook);
    }

    let repo = NotebookRepo {
        repo_id: paths.repo.repo_id.clone(),
        repo_kind: paths.repo.repo_kind.clone(),
        created_at: None,
        updated_at: None,
    };
    Ok(AgentNotebook {
        version: NOTEBOOK_VERSION,
        repo,
        anchors: Vec::new(),
        runbooks: Vec::new(),
    })
}

pub(crate) fn save_notebook(paths: &NotebookPaths, notebook: &AgentNotebook) -> Result<()> {
    if let Some(parent) = paths.notebook_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create notebook dir {}", parent.display()))?;
    }

    let bytes = serde_json::to_vec_pretty(notebook).context("serialize notebook")?;
    write_atomic(&paths.notebook_path, &bytes)?;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("notebook path has no parent")?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("notebook"),
        std::process::id()
    ));

    {
        let mut file =
            File::create(&tmp).with_context(|| format!("create tmp {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("write tmp {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync tmp {}", tmp.display()))?;
    }

    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename tmp {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

pub(crate) fn fill_missing_source_hashes(root: &Path, anchor: &mut NotebookAnchor) -> Result<()> {
    let mut file_hashes: HashMap<String, String> = HashMap::new();
    for ptr in &mut anchor.evidence {
        if ptr.source_hash.is_some() {
            continue;
        }
        let rel = ptr.file.replace('\\', "/");
        let hash = file_hashes
            .get(&rel)
            .cloned()
            .or_else(|| compute_file_hash(root, &rel).ok())
            .unwrap_or_default();
        if !hash.is_empty() {
            file_hashes.insert(rel.clone(), hash.clone());
            ptr.source_hash = Some(hash);
        }
    }
    Ok(())
}

fn compute_file_hash(root: &Path, rel: &str) -> Result<String> {
    if is_potential_secret_path(rel) {
        anyhow::bail!("refusing to read potential secret path: {rel}");
    }
    let canonical = root
        .join(Path::new(rel))
        .canonicalize()
        .with_context(|| format!("resolve evidence path '{rel}'"))?;
    if !canonical.starts_with(root) {
        anyhow::bail!("evidence file '{rel}' is outside project root");
    }
    let bytes = std::fs::read(&canonical)
        .with_context(|| format!("read file bytes {}", canonical.display()))?;
    Ok(sha256_hex(&bytes))
}

pub(crate) fn staleness_for_anchor(root: &Path, anchor: &NotebookAnchor) -> Result<(u32, u32)> {
    let mut cache: HashMap<String, String> = HashMap::new();
    let mut total = 0u32;
    let mut stale = 0u32;
    for ptr in &anchor.evidence {
        total += 1;
        let Some(expected) = ptr.source_hash.as_deref() else {
            continue;
        };
        let rel = ptr.file.replace('\\', "/");
        let current = if let Some(v) = cache.get(&rel) {
            v.clone()
        } else {
            let v = compute_file_hash(root, &rel).unwrap_or_default();
            cache.insert(rel.clone(), v.clone());
            v
        };
        if !current.is_empty() && current != expected {
            stale += 1;
        }
    }
    Ok((total, stale))
}
