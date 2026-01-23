use super::super::util::unix_ms;
use getrandom::getrandom;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

mod persist;

use persist::{
    acquire_persist_lock_best_effort, cursor_store_persist_path, load_persisted_best_effort,
    persist_persisted_best_effort, PersistedEntryData,
};

const CURSOR_STORE_CAPACITY: usize = 256;
const CURSOR_STORE_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
struct CursorStoreEntry {
    payload: Vec<u8>,
    expires_at: Instant,
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

    fn allocate_random_free_id(&self, mut is_free: impl FnMut(u64) -> bool) -> Option<u64> {
        for _ in 0..8 {
            let Some(candidate) = Self::random_u64_best_effort() else {
                break;
            };
            if is_free(candidate) {
                return Some(candidate);
            }
        }
        None
    }

    fn allocate_sequential_free_id(
        &self,
        mut candidate: u64,
        mut is_free: impl FnMut(u64) -> bool,
    ) -> u64 {
        candidate = candidate.max(1);
        while !is_free(candidate) {
            candidate = candidate.wrapping_add(1).max(1);
        }
        candidate
    }

    fn reserve_next_id(&mut self, id: u64) -> u64 {
        let id = id.max(1);
        self.next_id = id.wrapping_add(1).max(1);
        id
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

        let Some(_lock) = acquire_persist_lock_best_effort(&path) else {
            // If we cannot safely persist shared cursor aliases, prefer an in-memory-only insert.
            // This avoids cross-process collisions at the cost of losing persistence.
            return self.insert(payload);
        };

        let now_instant = Instant::now();
        self.prune_expired(now_instant);

        let now_unix_ms = unix_ms(SystemTime::now());
        let (mut order, mut entries, disk_max_id) = load_persisted_best_effort(&path, now_unix_ms);

        let id = self.allocate_persisted_id(&entries, disk_max_id);
        self.insert_entry(
            id,
            CursorStoreEntry {
                payload,
                expires_at: now_instant + CURSOR_STORE_TTL,
            },
        );
        self.merge_memory_into_persisted(now_instant, now_unix_ms, &mut order, &mut entries);
        Self::prune_persisted_to_capacity(&mut order, &mut entries);

        persist_persisted_best_effort(&path, &order, &entries);
        id
    }

    fn allocate_persisted_id(
        &mut self,
        disk_entries: &HashMap<u64, PersistedEntryData>,
        disk_max_id: u64,
    ) -> u64 {
        // Allocate an ID under the persistence lock to avoid collisions across processes.
        // Prefer random IDs: compact cursors are frequently copy-pasted across sessions, so
        // predictable low IDs (1,2,3,...) increase the chance that a cursor token accidentally
        // resolves to the wrong continuation in a different process.
        if let Some(id) = self.allocate_random_free_id(|candidate| {
            !disk_entries.contains_key(&candidate) && !self.entries.contains_key(&candidate)
        }) {
            return self.reserve_next_id(id);
        }

        let start = self.next_id.max(disk_max_id.wrapping_add(1).max(1)).max(1);
        let id = self.allocate_sequential_free_id(start, |candidate| {
            !disk_entries.contains_key(&candidate) && !self.entries.contains_key(&candidate)
        });
        self.reserve_next_id(id)
    }

    fn merge_memory_into_persisted(
        &self,
        now_instant: Instant,
        now_unix_ms: u64,
        order: &mut VecDeque<u64>,
        entries: &mut HashMap<u64, PersistedEntryData>,
    ) {
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
                PersistedEntryData {
                    payload: entry.payload.clone(),
                    expires_at_unix_ms,
                },
            );
            order.retain(|k| k != mem_id);
            order.push_back(*mem_id);
        }
    }

    fn prune_persisted_to_capacity(
        order: &mut VecDeque<u64>,
        entries: &mut HashMap<u64, PersistedEntryData>,
    ) {
        while order.len() > CURSOR_STORE_CAPACITY {
            if let Some(evicted) = order.pop_front() {
                entries.remove(&evicted);
            }
        }
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

        let id = self
            .allocate_random_free_id(|candidate| !self.entries.contains_key(&candidate))
            .unwrap_or_else(|| {
                self.allocate_sequential_free_id(self.next_id, |candidate| {
                    !self.entries.contains_key(&candidate)
                })
            });
        let id = self.reserve_next_id(id);

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

    fn load_best_effort(&mut self) {
        let Some(path) = self.persist_path.as_ref() else {
            return;
        };

        let now_unix_ms = unix_ms(SystemTime::now());
        let now_instant = Instant::now();
        let (order, entries, max_id) = load_persisted_best_effort(path, now_unix_ms);
        if entries.is_empty() {
            return;
        }

        self.entries.clear();
        self.order.clear();

        for id in &order {
            let Some(entry) = entries.get(id) else {
                continue;
            };
            let remaining_ms = entry.expires_at_unix_ms.saturating_sub(now_unix_ms);
            let expires_at = now_instant + Duration::from_millis(remaining_ms);
            self.entries.insert(
                *id,
                CursorStoreEntry {
                    payload: entry.payload.clone(),
                    expires_at,
                },
            );
            self.order.push_back(*id);
        }

        if !self.entries.is_empty() {
            self.next_id = max_id.wrapping_add(1).max(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CursorStore;
    use std::collections::{HashMap, VecDeque};

    #[test]
    fn roundtrips_in_memory() {
        let mut store = CursorStore {
            next_id: 1,
            entries: HashMap::new(),
            order: VecDeque::new(),
            persist_path: None,
        };

        let id = store.insert(b"hello".to_vec());
        assert_eq!(store.get(id).as_deref(), Some(b"hello".as_slice()));
    }
}
