use anyhow::{Context, Result};
use blake3::Hasher;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::fs;

#[derive(Clone, Debug)]
pub struct CacheConfig {
    pub dir: PathBuf,
    pub ttl: Duration,
    pub backend: CacheBackend,
    pub capacity: usize,
}

#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub enum CacheBackend {
    File,
    Memory,
}

impl CacheConfig {
    pub fn ensure_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("Cannot create cache dir {}", self.dir.display()))
    }

    #[allow(dead_code)]
    pub fn with_defaults() -> Self {
        Self {
            dir: PathBuf::from(".agents/mcp/context/.context/cache"),
            ttl: Duration::from_secs(86_400),
            backend: CacheBackend::Memory,
            capacity: 32,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct CacheEnvelope<T> {
    created_ms: u64,
    store_mtime_ms: u64,
    data: T,
}

pub async fn load_compare<T: for<'de> Deserialize<'de> + Clone>(
    cfg: &CacheConfig,
    key: &str,
    store_mtime_ms: u64,
) -> Result<Option<T>> {
    if cfg.backend == CacheBackend::Memory {
        return Ok(MEM_CACHE.lock().expect("cache mutex poisoned").get(
            key,
            cfg.ttl,
            store_mtime_ms,
            |env: &CacheEnvelope<T>| env.store_mtime_ms == store_mtime_ms,
        ));
    }

    let path = cfg.dir.join(format!("compare_{key}.json"));
    let Ok(bytes) = fs::read(&path).await else {
        return Ok(None);
    };

    let envelope: CacheEnvelope<T> = match serde_json::from_slice(&bytes) {
        Ok(val) => val,
        Err(err) => {
            log::warn!("Compare cache corrupted {}: {err}", path.display());
            return Ok(None);
        }
    };

    if envelope.store_mtime_ms != store_mtime_ms {
        return Ok(None);
    }

    let age = unix_ms_now().saturating_sub(envelope.created_ms);
    let ttl_ms = u64::try_from(cfg.ttl.as_millis()).unwrap_or(u64::MAX);
    if age > ttl_ms {
        return Ok(None);
    }

    Ok(Some(envelope.data))
}

pub async fn save_compare<T: Serialize>(
    cfg: &CacheConfig,
    key: &str,
    store_mtime_ms: u64,
    data: &T,
) -> Result<()> {
    if cfg.backend == CacheBackend::Memory {
        let envelope: CacheEnvelope<serde_json::Value> = CacheEnvelope {
            created_ms: unix_ms_now(),
            store_mtime_ms,
            data: serde_json::to_value(data)?,
        };
        MEM_CACHE
            .lock()
            .expect("cache mutex poisoned")
            .insert(key, envelope, cfg.capacity);
        return Ok(());
    }

    cfg.ensure_dir()?;
    let path = cfg.dir.join(format!("compare_{key}.json"));
    let bytes = {
        let envelope = CacheEnvelope {
            created_ms: unix_ms_now(),
            store_mtime_ms,
            data,
        };
        serde_json::to_vec_pretty(&envelope)?
    };
    fs::write(&path, bytes).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn compare_cache_key(
    project: &Path,
    queries: &[String],
    limit: usize,
    strategy: &str,
    reuse_graph: bool,
    show_graph: bool,
    language: &str,
    index_mtime_ms: u64,
) -> String {
    let mut hasher = Hasher::new();
    hasher.update(project.to_string_lossy().as_bytes());
    hasher.update(
        format!("|{limit}|{strategy}|{reuse_graph}|{show_graph}|{language}|{index_mtime_ms}")
            .as_bytes(),
    );
    for q in queries {
        hasher.update(b"|");
        hasher.update(q.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

struct MemCache {
    map: HashMap<String, serde_json::Value>,
    order: VecDeque<String>,
}

impl MemCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn touch(&mut self, key: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_front(key.to_string());
    }

    fn insert<T: Serialize>(&mut self, key: &str, envelope: CacheEnvelope<T>, capacity: usize) {
        if let Ok(val) = serde_json::to_value(envelope) {
            self.map.insert(key.to_string(), val);
            self.touch(key);
            while self.order.len() > capacity {
                if let Some(old) = self.order.pop_back() {
                    self.map.remove(&old);
                }
            }
        }
    }

    fn get<T: for<'de> Deserialize<'de> + Clone, F: Fn(&CacheEnvelope<T>) -> bool>(
        &mut self,
        key: &str,
        ttl: Duration,
        store_mtime_ms: u64,
        predicate: F,
    ) -> Option<T> {
        let val = self.map.get(key)?.clone();
        let envelope: CacheEnvelope<T> = serde_json::from_value(val).ok()?;
        let ttl_ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
        if unix_ms_now().saturating_sub(envelope.created_ms) > ttl_ms {
            self.map.remove(key);
            return None;
        }
        if envelope.store_mtime_ms != store_mtime_ms {
            self.map.remove(key);
            return None;
        }
        if !predicate(&envelope) {
            return None;
        }
        self.touch(key);
        Some(envelope.data)
    }
}

static MEM_CACHE: once_cell::sync::Lazy<Mutex<MemCache>> =
    once_cell::sync::Lazy::new(|| Mutex::new(MemCache::new()));
