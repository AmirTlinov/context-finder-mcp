use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_INDEX_CONCURRENCY: usize = 32;

static INDEX_CONCURRENCY_LIMIT: OnceLock<usize> = OnceLock::new();
static INDEX_CONCURRENCY_WAITERS: AtomicUsize = AtomicUsize::new(0);
static INDEX_CONCURRENCY_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexConcurrencySnapshot {
    pub limit: usize,
    pub in_flight: usize,
    pub waiters: usize,
}

fn total_memory_gib_linux_best_effort() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in contents.lines() {
        let line = line.trim_start();
        if !line.starts_with("MemTotal:") {
            continue;
        }
        let kb = line
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse::<u64>().ok())?;
        return Some(kb / 1024 / 1024);
    }
    None
}

fn default_index_concurrency() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let cpu_default = if cpus <= 4 {
        1
    } else if cpus <= 12 {
        2
    } else {
        3
    };

    let Some(mem_gib) = total_memory_gib_linux_best_effort() else {
        return cpu_default;
    };

    let mem_default = if mem_gib <= 8 {
        1
    } else if mem_gib <= 32 {
        2
    } else {
        3
    };

    cpu_default.min(mem_default).max(1)
}

fn parse_index_concurrency(raw: Option<&str>, default_value: usize) -> usize {
    raw.map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default_value)
        .clamp(1, MAX_INDEX_CONCURRENCY)
}

fn index_concurrency_from_env() -> usize {
    let raw = std::env::var("CONTEXT_FINDER_INDEX_CONCURRENCY").ok();
    parse_index_concurrency(raw.as_deref(), default_index_concurrency())
}

fn semaphore() -> Arc<Semaphore> {
    static SEM: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEM.get_or_init(|| {
        let limit = index_concurrency_from_env();
        let _ = INDEX_CONCURRENCY_LIMIT.set(limit);
        Arc::new(Semaphore::new(limit))
    })
    .clone()
}

pub fn index_concurrency_snapshot() -> IndexConcurrencySnapshot {
    let limit = *INDEX_CONCURRENCY_LIMIT.get_or_init(index_concurrency_from_env);
    IndexConcurrencySnapshot {
        limit,
        in_flight: INDEX_CONCURRENCY_IN_FLIGHT.load(Ordering::Relaxed),
        waiters: INDEX_CONCURRENCY_WAITERS.load(Ordering::Relaxed),
    }
}

pub(crate) struct IndexingPermit {
    #[allow(dead_code)]
    permit: OwnedSemaphorePermit,
}

impl Drop for IndexingPermit {
    fn drop(&mut self) {
        INDEX_CONCURRENCY_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

struct IndexingWaiterGuard;

impl IndexingWaiterGuard {
    fn new() -> Self {
        INDEX_CONCURRENCY_WAITERS.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for IndexingWaiterGuard {
    fn drop(&mut self) {
        INDEX_CONCURRENCY_WAITERS.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) async fn acquire_indexing_permit() -> IndexingPermit {
    // The semaphore is never closed; acquire failures are not expected.
    let waiter = IndexingWaiterGuard::new();
    let permit = semaphore()
        .acquire_owned()
        .await
        .unwrap_or_else(|_| unreachable!("index concurrency semaphore closed"));
    drop(waiter);
    INDEX_CONCURRENCY_IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
    IndexingPermit { permit }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_index_concurrency_defaults_and_clamps() {
        let default_value = default_index_concurrency();
        assert_eq!(parse_index_concurrency(None, default_value), default_value);
        assert_eq!(
            parse_index_concurrency(Some(""), default_value),
            default_value
        );
        assert_eq!(
            parse_index_concurrency(Some("   "), default_value),
            default_value
        );
        assert_eq!(parse_index_concurrency(Some("2"), default_value), 2);
        assert_eq!(parse_index_concurrency(Some("0"), default_value), 1);
        assert_eq!(
            parse_index_concurrency(Some("999"), default_value),
            MAX_INDEX_CONCURRENCY
        );
        assert_eq!(
            parse_index_concurrency(Some("abc"), default_value),
            default_value
        );
        assert_eq!(parse_index_concurrency(Some(" 5 "), default_value), 5);
    }
}
