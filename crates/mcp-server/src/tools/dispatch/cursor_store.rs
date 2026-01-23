use super::super::util::unix_ms;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use fs2::FileExt;
use getrandom::getrandom;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use context_vector_store::{CONTEXT_DIR_NAME, LEGACY_CONTEXT_DIR_NAME};

const CURSOR_STORE_CAPACITY: usize = 256;
const CURSOR_STORE_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
struct CursorStoreEntry {
    payload: Vec<u8>,
    expires_at: Instant,
}

#[derive(Clone)]
struct PersistedCursorStoreEntryData {
    payload: Vec<u8>,
    expires_at_unix_ms: u64,
}

pub(super) struct CursorStore {
    next_id: u64,
    entries: HashMap<u64, CursorStoreEntry>,
    order: VecDeque<u64>,
    persist_path: Option<PathBuf>,
}

impl CursorStore {
    fn random_u64_best_effort() -> Option<u64> {
        let mut bytes = [0u8; 8];
        getrandom(&mut bytes).ok()?;
        Some(u64::from_be_bytes(bytes).max(1))
    }

    pub(super) fn new() -> Self {
        let seed = Self::random_u64_best_effort().unwrap_or(1).max(1);
        let mut store = Self {
            next_id: seed,
            entries: HashMap::new(),
            order: VecDeque::new(),
            persist_path: cursor_store_persist_path(),
        };
        store.load_best_effort();
        store
    }

    pub(super) fn get(&mut self, id: u64) -> Option<Vec<u8>> {
        let now = Instant::now();
        self.prune_expired(now);

        let entry = self.entries.remove(&id)?;
        if entry.expires_at <= now {
            self.order.retain(|k| k != &id);
            return None;
        }

        self.order.retain(|k| k != &id);
        self.order.push_back(id);
        let payload = entry.payload.clone();
        self.entries.insert(id, entry);
        Some(payload)
    }

    pub(super) fn insert_persisted_best_effort(&mut self, payload: Vec<u8>) -> u64 {
        let Some(path) = self.persist_path.clone() else {
            return self.insert(payload);
        };

        let Some(_lock) = Self::acquire_persist_lock_best_effort(&path) else {
            // If we cannot safely persist shared cursor aliases, prefer an in-memory-only insert.
            // This avoids cross-process collisions at the cost of losing persistence.
            return self.insert(payload);
        };

        let now_instant = Instant::now();
        self.prune_expired(now_instant);

        let now_unix_ms = unix_ms(SystemTime::now());
        let (mut order, mut entries, disk_max_id) =
            Self::load_persisted_best_effort(&path, now_unix_ms);

        // Allocate an ID under the persistence lock to avoid collisions across processes.
        // Prefer random IDs: compact cursors are frequently copy-pasted across sessions, so
        // predictable low IDs (1,2,3,...) increase the chance that a cursor token accidentally
        // resolves to the wrong continuation in a different process.
        let mut id: Option<u64> = None;
        for _ in 0..8 {
            let Some(candidate) = Self::random_u64_best_effort() else {
                break;
            };
            if !entries.contains_key(&candidate) && !self.entries.contains_key(&candidate) {
                id = Some(candidate);
                break;
            }
        }
        let mut id = id.unwrap_or_else(|| {
            let mut candidate = self.next_id.max(disk_max_id.wrapping_add(1).max(1)).max(1);
            while entries.contains_key(&candidate) || self.entries.contains_key(&candidate) {
                candidate = candidate.wrapping_add(1).max(1);
            }
            candidate
        });
        while entries.contains_key(&id) || self.entries.contains_key(&id) {
            id = id.wrapping_add(1).max(1);
        }
        self.next_id = id.wrapping_add(1).max(1);

        self.insert_entry(
            id,
            CursorStoreEntry {
                payload,
                expires_at: now_instant + CURSOR_STORE_TTL,
            },
        );

        // Merge in-memory entries into the persisted view so we don't clobber other processes'
        // continuations when writing the file.
        for mem_id in &self.order {
            let Some(entry) = self.entries.get(mem_id) else {
                continue;
            };
            let remaining = entry.expires_at.saturating_duration_since(now_instant);
            let expires_at_unix_ms = now_unix_ms
                .saturating_add(u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX));
            entries.insert(
                *mem_id,
                PersistedCursorStoreEntryData {
                    payload: entry.payload.clone(),
                    expires_at_unix_ms,
                },
            );
            order.retain(|k| k != mem_id);
            order.push_back(*mem_id);
        }

        while order.len() > CURSOR_STORE_CAPACITY {
            if let Some(evicted) = order.pop_front() {
                entries.remove(&evicted);
            }
        }

        Self::persist_persisted_best_effort(&path, &order, &entries);

        id
    }

    fn insert_entry(&mut self, id: u64, entry: CursorStoreEntry) {
        self.entries.insert(id, entry);
        self.order.retain(|k| k != &id);
        self.order.push_back(id);

        while self.order.len() > CURSOR_STORE_CAPACITY {
            if let Some(evicted) = self.order.pop_front() {
                self.entries.remove(&evicted);
            }
        }
    }

    fn insert(&mut self, payload: Vec<u8>) -> u64 {
        let now = Instant::now();
        self.prune_expired(now);

        let mut id: Option<u64> = None;
        for _ in 0..8 {
            let Some(candidate) = Self::random_u64_best_effort() else {
                break;
            };
            if !self.entries.contains_key(&candidate) {
                id = Some(candidate);
                break;
            }
        }

        let mut id = id.unwrap_or_else(|| self.next_id.max(1));
        while self.entries.contains_key(&id) {
            id = id.wrapping_add(1).max(1);
        }
        self.next_id = id.wrapping_add(1).max(1);

        self.insert_entry(
            id,
            CursorStoreEntry {
                payload,
                expires_at: now + CURSOR_STORE_TTL,
            },
        );

        id
    }

    fn prune_expired(&mut self, now: Instant) {
        let mut expired: Vec<u64> = Vec::new();
        for (key, entry) in &self.entries {
            if entry.expires_at <= now {
                expired.push(*key);
            }
        }

        for key in expired {
            self.entries.remove(&key);
        }
        self.order.retain(|key| self.entries.contains_key(key));
    }

    fn acquire_persist_lock_best_effort(path: &Path) -> Option<std::fs::File> {
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

    fn load_persisted_best_effort(
        path: &Path,
        now_unix_ms: u64,
    ) -> (
        VecDeque<u64>,
        HashMap<u64, PersistedCursorStoreEntryData>,
        u64,
    ) {
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
                PersistedCursorStoreEntryData {
                    payload,
                    expires_at_unix_ms: entry.expires_at_unix_ms,
                },
            );
            if seen_ids.insert(entry.id) {
                order.push_back(entry.id);
            }
            max_id = max_id.max(entry.id);
        }

        while order.len() > CURSOR_STORE_CAPACITY {
            if let Some(evicted) = order.pop_front() {
                entries.remove(&evicted);
            }
        }

        (order, entries, max_id)
    }

    fn persist_persisted_best_effort(
        path: &Path,
        order: &VecDeque<u64>,
        entries: &HashMap<u64, PersistedCursorStoreEntryData>,
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

    fn load_best_effort(&mut self) {
        let Some(path) = self.persist_path.as_ref() else {
            return;
        };

        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(_) => return,
        };

        let persisted: PersistedCursorStore = match serde_json::from_slice(&bytes) {
            Ok(persisted) => persisted,
            Err(_) => return,
        };
        if persisted.v != 1 {
            return;
        }

        let now_unix_ms = unix_ms(SystemTime::now());
        let now_instant = Instant::now();

        let mut max_id = 0u64;
        let mut seen_ids: HashSet<u64> = HashSet::new();
        for entry in persisted.entries {
            if entry.expires_at_unix_ms <= now_unix_ms {
                continue;
            }
            let Ok(payload) = STANDARD.decode(entry.payload_b64.as_bytes()) else {
                continue;
            };
            let remaining_ms = entry.expires_at_unix_ms.saturating_sub(now_unix_ms);
            let expires_at = now_instant + Duration::from_millis(remaining_ms);
            self.entries.insert(
                entry.id,
                CursorStoreEntry {
                    payload,
                    expires_at,
                },
            );
            if seen_ids.insert(entry.id) {
                self.order.push_back(entry.id);
            }
            max_id = max_id.max(entry.id);
        }

        if !self.entries.is_empty() {
            self.next_id = max_id.wrapping_add(1).max(1);
        }

        while self.order.len() > CURSOR_STORE_CAPACITY {
            if let Some(evicted) = self.order.pop_front() {
                self.entries.remove(&evicted);
            }
        }
    }
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

fn cursor_store_persist_path() -> Option<PathBuf> {
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
