use crate::scanner::FileScanner;
use crate::{IndexerError, Result, Watermark};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::max;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{timeout, Duration};

const INDEX_WATERMARK_FILE_NAME: &str = "watermark.json";

// Watermark computation must be cheap and bounded. Some repos (dataset-heavy / many untracked files)
// can make `git status` extremely slow; in those cases we fall back to a filesystem watermark.
const GIT_HEAD_TIMEOUT: Duration = Duration::from_millis(1_000);
const GIT_STATUS_TIMEOUT: Duration = Duration::from_millis(2_000);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedIndexWatermark {
    pub built_at_unix_ms: u64,
    pub watermark: Watermark,
}

pub fn index_watermark_path_for_store(store_path: &Path) -> Result<PathBuf> {
    let dir = store_path
        .parent()
        .ok_or_else(|| IndexerError::InvalidPath("store path has no parent".into()))?;
    Ok(dir.join(INDEX_WATERMARK_FILE_NAME))
}

pub async fn write_index_watermark(store_path: &Path, watermark: Watermark) -> Result<()> {
    let path = index_watermark_path_for_store(store_path)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let built_at_unix_ms = unix_now_ms();
    let persisted = PersistedIndexWatermark {
        built_at_unix_ms,
        watermark,
    };

    let bytes = serde_json::to_vec_pretty(&persisted)?;
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

pub async fn read_index_watermark(store_path: &Path) -> Result<Option<PersistedIndexWatermark>> {
    let path = index_watermark_path_for_store(store_path)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = tokio::fs::read(&path).await?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub async fn compute_project_watermark(project_root: &Path) -> Result<Watermark> {
    if let Some(mark) = try_compute_git_watermark(project_root).await {
        return Ok(mark);
    }
    compute_filesystem_watermark(project_root).await
}

#[derive(Debug, Clone)]
pub(crate) struct GitState {
    pub computed_at_unix_ms: u64,
    pub git_head: String,
    pub git_dirty: bool,
    pub dirty_hash: Option<u64>,
    pub dirty_paths: Vec<PathBuf>,
}

pub(crate) async fn probe_git_state(project_root: &Path) -> Option<GitState> {
    let head = timeout(
        GIT_HEAD_TIMEOUT,
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(project_root)
            .arg("rev-parse")
            .arg("HEAD")
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !head.status.success() {
        return None;
    }
    let git_head = String::from_utf8_lossy(&head.stdout).trim().to_string();
    if git_head.is_empty() {
        return None;
    }

    let status = timeout(
        GIT_STATUS_TIMEOUT,
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(project_root)
            .arg("status")
            .arg("--porcelain")
            .arg("-z")
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !status.status.success() {
        return None;
    }
    let git_dirty = !status.stdout.is_empty();
    let (dirty_hash, dirty_paths) = if git_dirty {
        const MAX_DIRTY_PATHS_FOR_HASH: usize = 512;

        let mut hasher = Sha256::new();
        hasher.update(&status.stdout);

        // `git status --porcelain -z` is stable and cheap, but the raw output doesn't change when a
        // dirty file is modified again (it stays "M file"). To make freshness robust for dirty
        // repos, we mix in filesystem mtimes/sizes for the dirty paths (bounded).
        let tokens: Vec<&[u8]> = status
            .stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .collect();
        let mut dirty_paths: Vec<&[u8]> = Vec::new();
        let mut idx = 0usize;
        while idx < tokens.len() && dirty_paths.len() < MAX_DIRTY_PATHS_FOR_HASH {
            let token = tokens[idx];
            if token.len() < 4 || token.get(2) != Some(&b' ') {
                idx = idx.saturating_add(1);
                continue;
            }
            let status0 = token[0];
            let path1 = &token[3..];
            dirty_paths.push(path1);

            // For renames/copies, porcelain emits: `R  old\0new\0`.
            if (status0 == b'R' || status0 == b'C') && idx + 1 < tokens.len() {
                dirty_paths.push(tokens[idx + 1]);
                idx = idx.saturating_add(2);
            } else {
                idx = idx.saturating_add(1);
            }
        }

        let mut dirty_paths_buf: Vec<PathBuf> = Vec::with_capacity(dirty_paths.len());
        for path in dirty_paths {
            if path.is_empty() {
                continue;
            }
            hasher.update(path);

            let rel = String::from_utf8_lossy(path);
            dirty_paths_buf.push(PathBuf::from(rel.as_ref()));
            let candidate = project_root.join(Path::new(rel.as_ref()));
            if let Ok(meta) = tokio::fs::metadata(&candidate).await {
                hasher.update(meta.len().to_be_bytes());
                if let Ok(modified) = meta.modified() {
                    let mtime_ms = modified
                        .duration_since(UNIX_EPOCH)
                        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                        .unwrap_or(0);
                    hasher.update(mtime_ms.to_be_bytes());
                } else {
                    hasher.update(0u64.to_be_bytes());
                }
            } else {
                hasher.update(0u64.to_be_bytes());
                hasher.update(0u64.to_be_bytes());
            }
        }

        let digest = hasher.finalize();
        let dirty_hash = Some(u64::from_be_bytes([
            digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
        ]));
        (dirty_hash, dirty_paths_buf)
    } else {
        (None, Vec::new())
    };

    Some(GitState {
        computed_at_unix_ms: unix_now_ms(),
        git_head,
        git_dirty,
        dirty_hash,
        dirty_paths,
    })
}

pub(crate) async fn probe_git_changed_paths_between_heads(
    project_root: &Path,
    old_head: &str,
    new_head: &str,
    max_paths: usize,
) -> Option<Vec<PathBuf>> {
    let old_head = old_head.trim();
    let new_head = new_head.trim();
    if old_head.is_empty() || new_head.is_empty() {
        return None;
    }
    if old_head == new_head {
        return Some(Vec::new());
    }

    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(project_root)
        .arg("diff")
        .arg("--name-status")
        .arg("-z")
        .arg(old_head)
        .arg(new_head)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let tokens: Vec<&[u8]> = output
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .collect();

    let mut changed: HashSet<PathBuf> = HashSet::new();
    let mut idx = 0usize;
    while idx < tokens.len() {
        let status = tokens[idx];
        idx = idx.saturating_add(1);

        if idx >= tokens.len() {
            break;
        }
        let path1 = tokens[idx];
        idx = idx.saturating_add(1);

        let path1 = String::from_utf8_lossy(path1);
        if !path1.is_empty() {
            changed.insert(PathBuf::from(path1.as_ref()));
        }

        let Some(first) = status.first() else {
            continue;
        };
        if *first == b'R' || *first == b'C' {
            if idx >= tokens.len() {
                break;
            }
            let path2 = tokens[idx];
            idx = idx.saturating_add(1);

            let path2 = String::from_utf8_lossy(path2);
            if !path2.is_empty() {
                changed.insert(PathBuf::from(path2.as_ref()));
            }
        }

        if changed.len() > max_paths {
            return None;
        }
    }

    Some(changed.into_iter().collect())
}

async fn try_compute_git_watermark(project_root: &Path) -> Option<Watermark> {
    let state = probe_git_state(project_root).await?;
    Some(Watermark::Git {
        computed_at_unix_ms: Some(state.computed_at_unix_ms),
        git_head: state.git_head,
        git_dirty: state.git_dirty,
        dirty_hash: state.dirty_hash,
    })
}

async fn compute_filesystem_watermark(project_root: &Path) -> Result<Watermark> {
    let root = project_root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let scanner = FileScanner::new(&root);
        let files = scanner.scan();

        let mut file_count = 0u64;
        let mut total_bytes = 0u64;
        let mut max_mtime_ms = 0u64;

        for path in files {
            let meta = std::fs::metadata(&path)?;
            file_count += 1;
            total_bytes = total_bytes.saturating_add(meta.len());
            if let Ok(modified) = meta.modified() {
                let mtime_ms = modified
                    .duration_since(UNIX_EPOCH)
                    .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                    .unwrap_or(0);
                max_mtime_ms = max(max_mtime_ms, mtime_ms);
            }
        }

        Ok::<_, IndexerError>(Watermark::Filesystem {
            computed_at_unix_ms: Some(unix_now_ms()),
            file_count,
            max_mtime_ms,
            total_bytes,
        })
    })
    .await
    .map_err(|e| IndexerError::Other(format!("failed to compute filesystem watermark: {e}")))?
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::probe_git_changed_paths_between_heads;
    use std::path::Path;
    use tokio::process::Command;

    async fn git(repo: &Path, args: &[&str]) -> (bool, String) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .await
            .expect("git command");
        let ok = out.status.success();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (ok, stdout)
    }

    async fn git_ok(repo: &Path, args: &[&str]) -> String {
        let (ok, stdout) = git(repo, args).await;
        assert!(ok, "git {:?} failed", args);
        stdout
    }

    #[tokio::test]
    async fn git_diff_between_heads_includes_renames() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let repo = dir.path();

        git_ok(repo, &["init"]).await;
        git_ok(repo, &["config", "user.email", "test@example.com"]).await;
        git_ok(repo, &["config", "user.name", "Test"]).await;

        tokio::fs::write(repo.join("a.txt"), "alpha\n")
            .await
            .expect("write a");
        git_ok(repo, &["add", "."]).await;
        git_ok(repo, &["commit", "-m", "c1"]).await;
        let c1 = git_ok(repo, &["rev-parse", "HEAD"]).await;

        git_ok(repo, &["mv", "a.txt", "b.txt"]).await;
        git_ok(repo, &["commit", "-am", "c2"]).await;
        let c2 = git_ok(repo, &["rev-parse", "HEAD"]).await;

        let mut changed = probe_git_changed_paths_between_heads(repo, &c1, &c2, 512)
            .await
            .expect("diff paths");
        changed.sort();

        assert!(changed.contains(&"a.txt".into()));
        assert!(changed.contains(&"b.txt".into()));
    }

    #[tokio::test]
    async fn git_diff_respects_max_paths_limit() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let repo = dir.path();

        git_ok(repo, &["init"]).await;
        git_ok(repo, &["config", "user.email", "test@example.com"]).await;
        git_ok(repo, &["config", "user.name", "Test"]).await;

        tokio::fs::write(repo.join("a.txt"), "alpha\n")
            .await
            .expect("write a");
        tokio::fs::write(repo.join("b.txt"), "bravo\n")
            .await
            .expect("write b");
        git_ok(repo, &["add", "."]).await;
        git_ok(repo, &["commit", "-m", "c1"]).await;
        let c1 = git_ok(repo, &["rev-parse", "HEAD"]).await;

        tokio::fs::write(repo.join("a.txt"), "alpha2\n")
            .await
            .expect("write a2");
        tokio::fs::write(repo.join("b.txt"), "bravo2\n")
            .await
            .expect("write b2");
        git_ok(repo, &["commit", "-am", "c2"]).await;
        let c2 = git_ok(repo, &["rev-parse", "HEAD"]).await;

        let changed = probe_git_changed_paths_between_heads(repo, &c1, &c2, 1).await;
        assert!(changed.is_none());
    }
}
