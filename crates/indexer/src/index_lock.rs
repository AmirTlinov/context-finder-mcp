use crate::{IndexerError, Result};
use context_vector_store::context_dir_for_project_root;
use fs2::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static INDEX_WRITE_LOCK_WAIT_MS_LAST: AtomicU64 = AtomicU64::new(0);
static INDEX_WRITE_LOCK_WAIT_MS_MAX: AtomicU64 = AtomicU64::new(0);

pub fn index_write_lock_wait_ms_last() -> u64 {
    INDEX_WRITE_LOCK_WAIT_MS_LAST.load(Ordering::Relaxed)
}

pub fn index_write_lock_wait_ms_max() -> u64 {
    INDEX_WRITE_LOCK_WAIT_MS_MAX.load(Ordering::Relaxed)
}

fn update_write_lock_wait_ms(wait_ms: u64) {
    INDEX_WRITE_LOCK_WAIT_MS_LAST.store(wait_ms, Ordering::Relaxed);
    let mut current = INDEX_WRITE_LOCK_WAIT_MS_MAX.load(Ordering::Relaxed);
    while wait_ms > current {
        match INDEX_WRITE_LOCK_WAIT_MS_MAX.compare_exchange(
            current,
            wait_ms,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

pub(crate) struct IndexWriteLock {
    #[allow(dead_code)]
    file: std::fs::File,
}

impl Drop for IndexWriteLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn lock_path_for_root(root: &Path) -> PathBuf {
    context_dir_for_project_root(root).join("index.lock")
}

pub(crate) async fn acquire_index_write_lock(root: &Path) -> Result<IndexWriteLock> {
    let path = lock_path_for_root(root);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let lock = tokio::task::spawn_blocking(move || -> Result<IndexWriteLock> {
        use std::fs::OpenOptions;

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| {
                IndexerError::Other(format!("open index lock {}: {err}", path.display()))
            })?;

        let start = Instant::now();
        file.lock_exclusive().map_err(|err| {
            IndexerError::Other(format!("acquire index lock {}: {err}", path.display()))
        })?;
        update_write_lock_wait_ms(start.elapsed().as_millis() as u64);

        Ok(IndexWriteLock { file })
    })
    .await
    .map_err(|err| IndexerError::Other(format!("join index lock task: {err}")))??;

    Ok(lock)
}
