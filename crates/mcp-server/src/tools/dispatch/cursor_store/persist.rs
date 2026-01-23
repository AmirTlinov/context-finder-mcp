use base64::{engine::general_purpose::STANDARD, Engine as _};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use context_vector_store::{CONTEXT_DIR_NAME, LEGACY_CONTEXT_DIR_NAME};

#[derive(Clone)]
pub(super) struct PersistedEntryData {
    pub(super) payload: Vec<u8>,
    pub(super) expires_at_unix_ms: u64,
}

pub(super) fn acquire_persist_lock_best_effort(path: &Path) -> Option<std::fs::File> {
    let lock_path = path.with_extension("lock");
    let parent = lock_path.parent()?;
    std::fs::create_dir_all(parent).ok()?;

    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .ok()?;
    file.lock_exclusive().ok()?;
    Some(file)
}

pub(super) fn load_persisted_best_effort(
    path: &Path,
    now_unix_ms: u64,
) -> (VecDeque<u64>, HashMap<u64, PersistedEntryData>, u64) {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return (VecDeque::new(), HashMap::new(), 0),
    };

    let persisted: PersistedCursorStore = match serde_json::from_slice(&bytes) {
        Ok(persisted) => persisted,
        Err(_) => return (VecDeque::new(), HashMap::new(), 0),
    };
    if persisted.v != 1 {
        return (VecDeque::new(), HashMap::new(), 0);
    }

    let mut order = VecDeque::new();
    let mut entries = HashMap::new();
    let mut max_id = 0u64;
    let mut seen_ids: HashSet<u64> = HashSet::new();

    for entry in persisted.entries {
        if entry.expires_at_unix_ms <= now_unix_ms {
            continue;
        }
        let Ok(payload) = STANDARD.decode(entry.payload_b64.as_bytes()) else {
            continue;
        };
        entries.insert(
            entry.id,
            PersistedEntryData {
                payload,
                expires_at_unix_ms: entry.expires_at_unix_ms,
            },
        );
        if seen_ids.insert(entry.id) {
            order.push_back(entry.id);
        }
        max_id = max_id.max(entry.id);
    }

    while order.len() > super::CURSOR_STORE_CAPACITY {
        if let Some(evicted) = order.pop_front() {
            entries.remove(&evicted);
        }
    }

    (order, entries, max_id)
}

pub(super) fn persist_persisted_best_effort(
    path: &Path,
    order: &VecDeque<u64>,
    entries: &HashMap<u64, PersistedEntryData>,
) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }

    let mut persisted_entries = Vec::new();
    for id in order {
        let Some(entry) = entries.get(id) else {
            continue;
        };
        persisted_entries.push(PersistedCursorStoreEntry {
            id: *id,
            expires_at_unix_ms: entry.expires_at_unix_ms,
            payload_b64: STANDARD.encode(&entry.payload),
        });
    }

    let persisted = PersistedCursorStore {
        v: 1,
        entries: persisted_entries,
    };
    let Ok(data) = serde_json::to_vec(&persisted) else {
        return;
    };

    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &data).is_err() {
        return;
    }
    let _ = std::fs::rename(&tmp, path);
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedCursorStore {
    v: u32,
    entries: Vec<PersistedCursorStoreEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedCursorStoreEntry {
    id: u64,
    expires_at_unix_ms: u64,
    payload_b64: String,
}

pub(super) fn cursor_store_persist_path() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("CONTEXT_MCP_CURSOR_STORE_PATH")
        .or_else(|_| std::env::var("CONTEXT_FINDER_MCP_CURSOR_STORE_PATH"))
    {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }

    let home = dirs::home_dir()?;
    let preferred = home
        .join(CONTEXT_DIR_NAME)
        .join("cache")
        .join("cursor_store_v1.json");
    if preferred.exists() {
        return Some(preferred);
    }
    let legacy = home
        .join(LEGACY_CONTEXT_DIR_NAME)
        .join("cache")
        .join("cursor_store_v1.json");
    if legacy.exists() {
        return Some(legacy);
    }
    Some(preferred)
}
