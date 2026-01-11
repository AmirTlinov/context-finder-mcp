use crate::error::{Result, VectorStoreError};
use crate::gpu_env;
use crate::paths::{CONTEXT_CACHE_DIR_NAME, LEGACY_CONTEXT_CACHE_DIR_NAME};
use ndarray::{Array, Axis, Dimension, Ix2, Ix3};
use once_cell::sync::OnceCell;
use ort::execution_providers::{
    CPUExecutionProvider, CUDAExecutionProvider, ExecutionProvider, ExecutionProviderDispatch,
};
use ort::session::{builder::GraphOptimizationLevel, Input, Session, SessionInputs};
use ort::tensor::TensorElementType;
use ort::value::{DynTensor, Tensor};
use ort::Error as OrtError;
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fmt::Display;
use std::path::{Component, Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use tokenizers::{Encoding, PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};
use tokio::task::spawn_blocking;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum EmbeddingMode {
    Fast,
    Stub,
}

impl EmbeddingMode {
    fn from_env() -> Result<Self> {
        let raw = env::var("CONTEXT_EMBEDDING_MODE")
            .or_else(|_| env::var("CONTEXT_FINDER_EMBEDDING_MODE"))
            .unwrap_or_else(|_| "fast".to_string())
            .to_ascii_lowercase();
        match raw.as_str() {
            "fast" => Ok(Self::Fast),
            "stub" => Ok(Self::Stub),
            other => Err(VectorStoreError::EmbeddingError(format!(
                "Unsupported CONTEXT_EMBEDDING_MODE '{other}' (expected 'fast' or 'stub')"
            ))),
        }
    }
}

pub fn current_embedding_mode_id() -> Result<&'static str> {
    match EmbeddingMode::from_env()? {
        EmbeddingMode::Fast => Ok("fast"),
        EmbeddingMode::Stub => Ok("stub"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ModelId(String);

impl Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone)]
struct ModelSpec {
    id: ModelId,
    onnx_rel_path: PathBuf,
    tokenizer_rel_path: PathBuf,
    dimension: usize,
    max_length: usize,
    max_batch: usize,
}

impl ModelSpec {
    fn assets_in(&self, model_dir: &Path) -> ModelAssets {
        let model_dir = model_dir.join(self.id.to_string());
        ModelAssets {
            model_path: model_dir.join(&self.onnx_rel_path),
            tokenizer_path: model_dir.join(&self.tokenizer_rel_path),
        }
    }
}

fn validate_relative_manifest_path(path: &Path) -> Result<()> {
    let mut has_component = false;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                return Err(VectorStoreError::EmbeddingError(
                    "models manifest asset path must be relative".to_string(),
                ));
            }
            Component::ParentDir => {
                return Err(VectorStoreError::EmbeddingError(
                    "models manifest asset path must not contain '..'".to_string(),
                ));
            }
            Component::CurDir => {}
            Component::Normal(_) => {
                has_component = true;
            }
        }
    }

    if !has_component {
        return Err(VectorStoreError::EmbeddingError(
            "models manifest asset path is empty".to_string(),
        ));
    }

    Ok(())
}

fn safe_rel_path_from_manifest(model_id: &str, rel: &str) -> Result<PathBuf> {
    let path = Path::new(rel);
    validate_relative_manifest_path(path).map_err(|err| {
        VectorStoreError::EmbeddingError(format!(
            "Invalid models manifest asset path for model '{model_id}': '{rel}' ({err})"
        ))
    })?;
    Ok(path.to_path_buf())
}

#[derive(Clone)]
struct ModelAssets {
    model_path: PathBuf,
    tokenizer_path: PathBuf,
}

struct OrtBackend {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    max_length: usize,
    max_batch: usize,
    dimension: usize,
}

#[derive(Clone)]
struct StubBackend {
    dimension: usize,
    #[cfg(test)]
    batch_calls: Arc<AtomicUsize>,
}

impl StubBackend {
    #[cfg(not(test))]
    const fn new(dimension: usize) -> Self {
        Self { dimension }
    }

    #[cfg(test)]
    fn new(dimension: usize) -> Self {
        Self {
            dimension,
            #[cfg(test)]
            batch_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        #[cfg(test)]
        self.batch_calls.fetch_add(1, Ordering::Relaxed);
        texts
            .iter()
            .map(|text| stub_embed(text, self.dimension))
            .collect()
    }

    #[cfg(test)]
    fn batch_calls(&self) -> usize {
        self.batch_calls.load(Ordering::Relaxed)
    }
}

static BACKENDS: OnceCell<Mutex<BackendCache<OrtBackend>>> = OnceCell::new();

struct LoadWaiter<B> {
    state: Arc<(Mutex<LoadState<B>>, Condvar)>,
}

struct LoadState<B> {
    done: bool,
    backend: Option<Arc<B>>,
    error: Option<String>,
}

impl<B> Clone for LoadWaiter<B> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl<B> LoadWaiter<B> {
    fn new() -> Self {
        Self {
            state: Arc::new((
                Mutex::new(LoadState {
                    done: false,
                    backend: None,
                    error: None,
                }),
                Condvar::new(),
            )),
        }
    }

    fn set_ok(&self, backend: Arc<B>) {
        let (lock, cv) = &*self.state;
        {
            let mut guard = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.done = true;
            guard.backend = Some(backend);
            guard.error = None;
        }
        cv.notify_all();
    }

    fn set_err(&self, error: String) {
        let (lock, cv) = &*self.state;
        {
            let mut guard = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.done = true;
            guard.backend = None;
            guard.error = Some(error);
        }
        cv.notify_all();
    }

    fn wait(&self) -> Result<Arc<B>> {
        let (lock, cv) = &*self.state;
        let mut guard = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !guard.done {
            guard = cv
                .wait(guard)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if let Some(backend) = &guard.backend {
            return Ok(backend.clone());
        }
        Err(VectorStoreError::EmbeddingError(
            guard
                .error
                .clone()
                .unwrap_or_else(|| "Unknown backend load error".to_string()),
        ))
    }
}

#[derive(Clone)]
enum BackendEntry<B> {
    Ready(Arc<B>),
    Loading(LoadWaiter<B>),
}

struct BackendCache<B> {
    capacity: usize,
    entries: HashMap<ModelId, BackendEntry<B>>,
    lru: VecDeque<ModelId>,
}

impl<B> BackendCache<B> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: HashMap::new(),
            lru: VecDeque::new(),
        }
    }

    fn get_ready(&mut self, id: &ModelId) -> Option<Arc<B>> {
        let backend = match self.entries.get(id) {
            Some(BackendEntry::Ready(backend)) => backend.clone(),
            _ => return None,
        };
        self.touch(id);
        Some(backend)
    }

    fn touch(&mut self, id: &ModelId) {
        if let Some(pos) = self.lru.iter().position(|k| k == id) {
            self.lru.remove(pos);
        }
        self.lru.push_front(id.clone());
    }

    fn begin_load(&mut self, id: ModelId) -> LoadWaiter<B> {
        let waiter = LoadWaiter::new();
        self.entries
            .insert(id, BackendEntry::Loading(waiter.clone()));
        waiter
    }

    fn finish_ok(&mut self, id: &ModelId, backend: Arc<B>) {
        self.entries
            .insert(id.clone(), BackendEntry::Ready(backend));
        self.touch(id);
        self.evict_if_needed();
    }

    fn finish_err(&mut self, id: &ModelId) {
        self.entries.remove(id);
        if let Some(pos) = self.lru.iter().position(|k| k == id) {
            self.lru.remove(pos);
        }
    }

    fn evict_if_needed(&mut self) {
        while self.ready_len() > self.capacity {
            let Some(victim) = self.lru.pop_back() else {
                break;
            };
            if let Some(entry) = self.entries.get(&victim) {
                match entry {
                    BackendEntry::Ready(_) => {
                        self.entries.remove(&victim);
                    }
                    BackendEntry::Loading(_) => {
                        // Don't evict in-flight loads; put it back at the front.
                        self.lru.push_front(victim);
                        break;
                    }
                }
            }
        }
    }

    fn ready_len(&self) -> usize {
        self.entries
            .values()
            .filter(|e| matches!(e, BackendEntry::Ready(_)))
            .count()
    }
}

fn backend_cache_capacity_from_env() -> usize {
    std::env::var("CONTEXT_MODEL_REGISTRY_CAPACITY")
        .or_else(|_| std::env::var("CONTEXT_FINDER_MODEL_REGISTRY_CAPACITY"))
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(default_backend_cache_capacity)
        .max(1)
}

fn default_backend_cache_capacity() -> usize {
    let Some(mem_gib) = total_memory_gib_linux_best_effort() else {
        return 2;
    };

    if mem_gib <= 8 {
        1
    } else if mem_gib <= 16 {
        2
    } else if mem_gib <= 32 {
        3
    } else {
        4
    }
}

fn total_memory_gib_linux_best_effort() -> Option<u64> {
    #[cfg(not(target_os = "linux"))]
    {
        None
    }

    #[cfg(target_os = "linux")]
    {
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
}

pub fn model_dir() -> PathBuf {
    if let Ok(path) = std::env::var("CONTEXT_MODEL_DIR") {
        return PathBuf::from(path);
    }
    if let Ok(path) = std::env::var("CONTEXT_FINDER_MODEL_DIR") {
        return PathBuf::from(path);
    }

    // Prefer a repo-local `models/manifest.json` near the executable (agent-friendly, no hidden
    // caches). This allows running `context` from an arbitrary project directory while
    // still resolving the tool's own `./models/` folder.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(mut dir) = exe.parent().map(std::path::Path::to_path_buf) {
            loop {
                let candidate = dir.join("models");
                if candidate.join("manifest.json").exists() {
                    return candidate;
                }
                if !dir.pop() {
                    break;
                }
            }
        }
    }

    // Search upwards from the current directory as a fallback (e.g., when running from a
    // workspace checkout without installing the binary).
    if let Ok(mut dir) = std::env::current_dir() {
        loop {
            let candidate = dir.join("models");
            if candidate.join("manifest.json").exists() {
                return candidate;
            }
            if !dir.pop() {
                break;
            }
        }
    }

    if let Ok(path) = std::env::var("XDG_CACHE_HOME") {
        let base = PathBuf::from(path);
        let preferred = base.join(CONTEXT_CACHE_DIR_NAME).join("models");
        if preferred.exists() {
            return preferred;
        }
        let legacy = base.join(LEGACY_CONTEXT_CACHE_DIR_NAME).join("models");
        if legacy.exists() {
            return legacy;
        }
        return preferred;
    }

    let base = std::env::var("HOME")
        .map_or_else(|_| PathBuf::from("."), PathBuf::from)
        .join(".cache");
    let preferred = base.join(CONTEXT_CACHE_DIR_NAME).join("models");
    if preferred.exists() {
        return preferred;
    }
    let legacy = base.join(LEGACY_CONTEXT_CACHE_DIR_NAME).join("models");
    if legacy.exists() {
        return legacy;
    }
    preferred
}

impl ModelId {
    fn from_raw(model_name: &str) -> Self {
        let normalized = Self::normalize(model_name);
        Self(normalized)
    }

    fn from_env() -> Self {
        let model_name = std::env::var("CONTEXT_EMBEDDING_MODEL")
            .or_else(|_| std::env::var("CONTEXT_FINDER_EMBEDDING_MODEL"))
            .unwrap_or_else(|_| "bge-small".to_string());
        Self::from_raw(&model_name)
    }

    fn normalize(raw: &str) -> String {
        let model_name = raw.trim().to_ascii_lowercase();
        match model_name.as_str() {
            "bge-small-en-v1.5" => "bge-small".to_string(),
            "bge-base-en-v1.5" => "bge-base".to_string(),
            "bge-large-en-v1.5" => "bge-large".to_string(),
            other => other.to_string(),
        }
    }

    fn spec(&self) -> Result<ModelSpec> {
        let base = model_dir();
        let manifest_path = base.join("manifest.json");

        if manifest_path.exists() {
            let raw = std::fs::read_to_string(&manifest_path).map_err(|e| {
                VectorStoreError::EmbeddingError(format!(
                    "Failed to read models manifest {}: {e}",
                    manifest_path.display()
                ))
            })?;
            let manifest: ModelsManifest = serde_json::from_str(&raw).map_err(|e| {
                VectorStoreError::EmbeddingError(format!(
                    "Invalid models manifest {}: {e}",
                    manifest_path.display()
                ))
            })?;
            if manifest.schema_version != 1 {
                return Err(VectorStoreError::EmbeddingError(format!(
                    "Unsupported models manifest schema_version {} (expected 1)",
                    manifest.schema_version
                )));
            }

            let wanted = self.0.as_str();
            let model = manifest
                .models
                .iter()
                .find(|m| m.id.eq_ignore_ascii_case(wanted))
                .ok_or_else(|| {
                    let available = manifest
                        .models
                        .iter()
                        .map(|m| m.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    VectorStoreError::EmbeddingError(format!(
                        "Unknown embedding model id '{wanted}'. Available: {available}"
                    ))
                })?;

            let prefix = format!("{}/", model.id);
            let mut onnx_rel_path: Option<PathBuf> = None;
            let mut tokenizer_rel_path: Option<PathBuf> = None;
            for asset in &model.assets {
                if !asset.path.starts_with(&prefix) {
                    continue;
                }
                let rel = asset
                    .path
                    .strip_prefix(&prefix)
                    .unwrap_or(asset.path.as_str());
                let asset_path = std::path::Path::new(asset.path.as_str());
                if onnx_rel_path.is_none()
                    && asset_path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("onnx"))
                {
                    onnx_rel_path = Some(safe_rel_path_from_manifest(&model.id, rel)?);
                }
                if tokenizer_rel_path.is_none()
                    && asset_path
                        .file_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case("tokenizer.json"))
                {
                    tokenizer_rel_path = Some(safe_rel_path_from_manifest(&model.id, rel)?);
                }
            }

            return Ok(ModelSpec {
                id: self.clone(),
                onnx_rel_path: onnx_rel_path.unwrap_or_else(|| PathBuf::from("model.onnx")),
                tokenizer_rel_path: tokenizer_rel_path
                    .unwrap_or_else(|| PathBuf::from("tokenizer.json")),
                dimension: model.dimension,
                max_length: model.max_length,
                max_batch: model.max_batch,
            });
        }

        // Legacy fallback: keep bge-small working for users who only have the old cache layout.
        if self.0 == "bge-small" {
            return Ok(ModelSpec {
                id: self.clone(),
                onnx_rel_path: PathBuf::from("model.onnx"),
                tokenizer_rel_path: PathBuf::from("tokenizer.json"),
                dimension: 384,
                max_length: 512,
                max_batch: 32,
            });
        }

        Err(VectorStoreError::EmbeddingError(format!(
            "Unknown embedding model id '{}' and no models manifest found at {}",
            self.0,
            manifest_path.display()
        )))
    }
}

#[derive(Debug, Deserialize)]
struct ModelsManifest {
    schema_version: u32,
    models: Vec<ManifestModel>,
}

#[derive(Debug, Deserialize)]
struct ManifestModel {
    id: String,
    dimension: usize,
    max_length: usize,
    max_batch: usize,
    #[serde(default)]
    assets: Vec<ManifestAsset>,
}

#[derive(Debug, Deserialize)]
struct ManifestAsset {
    path: String,
}

impl OrtBackend {
    fn new(spec: &ModelSpec, model_dir: &Path) -> Result<Self> {
        // Tokenization can be a surprisingly large CPU tax during large indexing runs. By default,
        // prefer deterministic, low-contention behavior (single-threaded) unless the user opted
        // into parallel tokenization explicitly.
        if !tokenizers::utils::parallelism::is_parallelism_configured() {
            tokenizers::utils::parallelism::set_parallelism(false);
        }

        let assets = spec.assets_in(model_dir);
        if !assets.model_path.exists() || !assets.tokenizer_path.exists() {
            return Err(VectorStoreError::EmbeddingError(format!(
                "Model files for '{}' are missing. Expected ONNX at {} and tokenizer at {}. Run `context install-models` to download them into ./models (or set CONTEXT_MODEL_DIR).",
                spec.id,
                assets.model_path.display(),
                assets.tokenizer_path.display(),
            )));
        }

        let mut tokenizer = Tokenizer::from_file(&assets.tokenizer_path)
            .map_err(|e| VectorStoreError::EmbeddingError(format!("Tokenizer load failed: {e}")))?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..PaddingParams::default()
        }));
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: spec.max_length,
                ..TruncationParams::default()
            }))
            .map_err(|e| {
                VectorStoreError::EmbeddingError(format!("Tokenizer truncation failed: {e}"))
            })?;

        let providers = build_execution_providers()?;
        let (intra_threads, inter_threads) = default_ort_threads();
        let session_builder =
            Session::builder().map_err(|e| VectorStoreError::EmbeddingError(format!("{e}")))?;
        let session = session_builder
            // Keep inference "polite": cap thread usage and disable busy-spinning so background
            // indexing doesn't steal CPU from interactive work.
            .with_intra_threads(intra_threads)
            .map_err(|e| {
                VectorStoreError::EmbeddingError(format!("Failed to set ORT intra threads: {e}"))
            })?
            .with_inter_threads(inter_threads)
            .map_err(|e| {
                VectorStoreError::EmbeddingError(format!("Failed to set ORT inter threads: {e}"))
            })?
            .with_intra_op_spinning(false)
            .map_err(|e| {
                VectorStoreError::EmbeddingError(format!("Failed to set ORT intra spinning: {e}"))
            })?
            .with_inter_op_spinning(false)
            .map_err(|e| {
                VectorStoreError::EmbeddingError(format!("Failed to set ORT inter spinning: {e}"))
            })?
            .with_execution_providers(providers)
            .map_err(|e| {
                VectorStoreError::EmbeddingError(format!(
                    "Failed to register CUDA execution provider: {e}"
                ))
            })?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| {
                VectorStoreError::EmbeddingError(format!("Failed to set optimization level: {e}"))
            })?
            .commit_from_file(&assets.model_path)
            .map_err(|e| {
                VectorStoreError::EmbeddingError(format!("Failed to load ONNX model: {e}"))
            })?;

        log::info!(
            "Loaded ONNX model '{}' (dim {}, max_length {}, batch {})",
            spec.id,
            spec.dimension,
            spec.max_length,
            spec.max_batch
        );

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            max_length: spec.max_length,
            max_batch: spec.max_batch,
            dimension: spec.dimension,
        })
    }

    fn embed_batch_blocking(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut results = Vec::with_capacity(texts.len());
        for batch in texts.chunks(self.max_batch) {
            let encodings = self
                .tokenizer
                .encode_batch(batch.to_vec(), true)
                .map_err(|e| {
                    VectorStoreError::EmbeddingError(format!("Tokenization failed: {e}"))
                })?;

            if encodings.is_empty() {
                continue;
            }

            let seq_len = encodings[0].len();
            if seq_len > self.max_length {
                return Err(VectorStoreError::EmbeddingError(format!(
                    "Tokenized length {} exceeds max_length {}",
                    seq_len, self.max_length
                )));
            }
            if encodings.iter().any(|e| e.len() != seq_len) {
                return Err(VectorStoreError::EmbeddingError(
                    "Inconsistent sequence lengths after padding".to_string(),
                ));
            }
            let (ids, masks, type_ids, mask_rows) = build_flat_tensors(&encodings, seq_len);

            let ids_array = Array::from_shape_vec((batch.len(), seq_len), ids)
                .map_err(|e| VectorStoreError::EmbeddingError(format!("IDs shape error: {e}")))?;
            let mask_array = Array::from_shape_vec((batch.len(), seq_len), masks)
                .map_err(|e| VectorStoreError::EmbeddingError(format!("Mask shape error: {e}")))?;
            let type_array = Array::from_shape_vec((batch.len(), seq_len), type_ids)
                .map_err(|e| VectorStoreError::EmbeddingError(format!("Types shape error: {e}")))?;
            let ids_shape = ids_array.raw_dim().into_dyn();

            let ids_tensor = Tensor::from_array(ids_array.into_dyn())
                .map_err(|e| to_embedding_error(&e))?
                .upcast();
            let mask_tensor = Tensor::from_array(mask_array.into_dyn())
                .map_err(|e| to_embedding_error(&e))?
                .upcast();
            let type_tensor = Tensor::from_array(type_array.into_dyn())
                .map_err(|e| to_embedding_error(&e))?
                .upcast();

            let array = {
                let mut session = self.session.lock().map_err(|_| {
                    VectorStoreError::EmbeddingError("Failed to lock ONNX session".into())
                })?;

                let mut available: HashMap<String, DynTensor> = HashMap::new();
                available.insert("input_ids".to_string(), ids_tensor);
                available.insert("attention_mask".to_string(), mask_tensor);
                available.insert("token_type_ids".to_string(), type_tensor);

                let mut feed: HashMap<String, DynTensor> = HashMap::new();

                for input in &session.inputs {
                    let key = input.name.clone();
                    if let Some(value) = available.get(&key) {
                        feed.insert(key, value.clone());
                    } else {
                        let zeros = zero_tensor(&ids_shape, input).map_err(|e| {
                            VectorStoreError::EmbeddingError(format!(
                                "Unsupported ONNX input '{key}': {e}"
                            ))
                        })?;
                        feed.insert(key, zeros);
                    }
                }

                let outputs = session.run(SessionInputs::from(feed)).map_err(|e| {
                    VectorStoreError::EmbeddingError(format!("ONNX forward failed: {e}"))
                })?;

                if outputs.len() == 0 {
                    return Err(VectorStoreError::EmbeddingError(
                        "ONNX returned no outputs".to_string(),
                    ));
                }

                let array = outputs[0]
                    .try_extract_array::<f32>()
                    .map_err(|e| {
                        VectorStoreError::EmbeddingError(format!(
                            "Failed to decode ONNX output: {e}"
                        ))
                    })?
                    .to_owned();

                drop(outputs);
                drop(session);

                array
            };
            results.extend(embeddings_from_output(array, &mask_rows, self.dimension)?);
        }

        Ok(results)
    }
}

fn default_ort_threads() -> (usize, usize) {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    // Inference parallelism is a throughput knob. For agent workflows, tail latency and "polite"
    // coexistence matter more than max throughput, especially in the shared daemon.
    let intra_threads = if cpus <= 4 {
        1
    } else if cpus <= 12 {
        2
    } else if cpus <= 24 {
        3
    } else {
        4
    };

    // Default is sequential execution mode; keep inter-op conservative.
    (intra_threads.max(1), 1)
}

const fn ensure_dimension(vec: &[f32], expected: usize) -> Result<()> {
    if vec.len() != expected {
        return Err(VectorStoreError::InvalidDimension {
            expected,
            actual: vec.len(),
        });
    }
    Ok(())
}

fn embeddings_from_output(
    array: ndarray::ArrayD<f32>,
    mask_rows: &[Vec<i64>],
    expected_dimension: usize,
) -> Result<Vec<Vec<f32>>> {
    let mut out = Vec::new();
    match array.ndim() {
        2 => {
            let embeddings = array
                .into_dimensionality::<Ix2>()
                .map_err(|e| VectorStoreError::EmbeddingError(format!("Bad output shape: {e}")))?;
            out.reserve(embeddings.len_of(Axis(0)));
            for row in embeddings.outer_iter() {
                let mut emb = row.to_owned().to_vec();
                ensure_dimension(&emb, expected_dimension)?;
                normalize(&mut emb);
                out.push(emb);
            }
        }
        3 => {
            let hidden = array
                .into_dimensionality::<Ix3>()
                .map_err(|e| VectorStoreError::EmbeddingError(format!("Bad output shape: {e}")))?;
            out.reserve(hidden.len_of(Axis(0)));
            for (idx, sample) in hidden.outer_iter().enumerate() {
                let attn = mask_rows
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| vec![1; sample.len_of(Axis(0))]);
                let pooled = mean_pool(sample.view(), &attn);
                let mut emb = pooled;
                ensure_dimension(&emb, expected_dimension)?;
                normalize(&mut emb);
                out.push(emb);
            }
        }
        _ => {
            return Err(VectorStoreError::EmbeddingError(format!(
                "Unexpected ONNX output dims: {:?}",
                array.shape()
            )));
        }
    }
    Ok(out)
}

fn mean_pool(sample: ndarray::ArrayView2<'_, f32>, mask: &[i64]) -> Vec<f32> {
    if sample.is_empty() {
        return vec![];
    }

    let hidden = sample.len_of(Axis(1));
    let mut sum = vec![0.0f32; hidden];
    let mut count = 0.0f32;

    for (token_idx, token) in sample.outer_iter().enumerate() {
        if *mask.get(token_idx).unwrap_or(&0) == 0 {
            continue;
        }
        count += 1.0;
        for (dim, value) in token.iter().enumerate() {
            sum[dim] += value;
        }
    }

    if count == 0.0 {
        return sum;
    }

    for value in &mut sum {
        *value /= count;
    }

    sum
}

fn build_flat_tensors(
    encodings: &[Encoding],
    seq_len: usize,
) -> (Vec<i64>, Vec<i64>, Vec<i64>, Vec<Vec<i64>>) {
    let mut ids = Vec::with_capacity(encodings.len() * seq_len);
    let mut masks = Vec::with_capacity(encodings.len() * seq_len);
    let mut type_ids = Vec::with_capacity(encodings.len() * seq_len);
    let mut mask_rows = Vec::with_capacity(encodings.len());

    for encoding in encodings {
        let encoding_ids = encoding.get_ids();
        let encoding_masks = encoding.get_attention_mask();
        let encoding_types = encoding.get_type_ids();

        for idx in 0..seq_len {
            ids.push(i64::from(*encoding_ids.get(idx).unwrap_or(&0)));
            masks.push(i64::from(*encoding_masks.get(idx).unwrap_or(&0)));
            type_ids.push(i64::from(*encoding_types.get(idx).unwrap_or(&0)));
        }

        mask_rows.push(
            encoding_masks
                .iter()
                .take(seq_len)
                .map(|v| i64::from(*v))
                .collect(),
        );
    }

    (ids, masks, type_ids, mask_rows)
}

fn normalize(vec: &mut [f32]) {
    let norm = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm == 0.0 {
        return;
    }
    for value in vec {
        *value /= norm;
    }
}

fn stub_embed(text: &str, dimension: usize) -> Vec<f32> {
    let mut state =
        fnv1a_64(text.as_bytes()) ^ (dimension as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut vec = Vec::with_capacity(dimension);
    for _ in 0..dimension {
        let bits = splitmix64(&mut state);
        let high = (bits >> 32) as u32;
        let mantissa = high >> 9;
        let unit = f32::from_bits(0x3f80_0000 | mantissa) - 1.0;
        vec.push(unit.mul_add(2.0, -1.0));
    }
    normalize(&mut vec);
    vec
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

const fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn to_embedding_error(error: &OrtError) -> VectorStoreError {
    VectorStoreError::EmbeddingError(format!("{error}"))
}

fn is_cuda_disabled() -> bool {
    env::var("ORT_DISABLE_CUDA")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
        || env::var("ORT_USE_CUDA")
            .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
            .unwrap_or(false)
}

fn allow_cpu_fallback() -> bool {
    env::var("CONTEXT_ALLOW_CPU")
        .or_else(|_| env::var("CONTEXT_FINDER_ALLOW_CPU"))
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn build_execution_providers() -> Result<Vec<ExecutionProviderDispatch>> {
    if is_cuda_disabled() {
        if allow_cpu_fallback() {
            return Ok(vec![CPUExecutionProvider::default().build()]);
        }
        return Err(VectorStoreError::EmbeddingError(
            "CUDA is disabled (ORT_DISABLE_CUDA/ORT_USE_CUDA), but CPU fallback is not allowed. Set CONTEXT_ALLOW_CPU=1 to allow CPU embeddings."
                .to_string(),
        ));
    }

    match build_cuda_ep() {
        Ok(cuda) => Ok(vec![cuda]),
        Err(err) => {
            if allow_cpu_fallback() {
                log::warn!("CUDA EP unavailable, falling back to CPU embeddings: {err}");
                Ok(vec![CPUExecutionProvider::default().build()])
            } else {
                Err(VectorStoreError::EmbeddingError(format!(
                    "CUDA execution provider is unavailable: {err}. Run with CONTEXT_ALLOW_CPU=1 to allow CPU embeddings."
                )))
            }
        }
    }
}

fn build_cuda_ep() -> Result<ort::execution_providers::ExecutionProviderDispatch> {
    let report = gpu_env::bootstrap_cuda_env_best_effort();
    let mut cuda = CUDAExecutionProvider::default();

    if let Ok(device) =
        env::var("CONTEXT_CUDA_DEVICE").or_else(|_| env::var("CONTEXT_FINDER_CUDA_DEVICE"))
    {
        let parsed: i32 = device.parse().map_err(|e| {
            VectorStoreError::EmbeddingError(format!("Invalid CONTEXT_CUDA_DEVICE '{device}': {e}"))
        })?;
        cuda = cuda.with_device_id(parsed);
    }

    if let Ok(limit_mb) = env::var("CONTEXT_CUDA_MEM_LIMIT_MB")
        .or_else(|_| env::var("CONTEXT_FINDER_CUDA_MEM_LIMIT_MB"))
    {
        let parsed: usize = limit_mb.parse().map_err(|e| {
            VectorStoreError::EmbeddingError(format!(
                "Invalid CONTEXT_CUDA_MEM_LIMIT_MB '{limit_mb}': {e}"
            ))
        })?;
        cuda = cuda.with_memory_limit(parsed * 1024 * 1024);
    }

    match cuda.is_available() {
        Ok(true) => {}
        Ok(false) => {
            return Err(VectorStoreError::EmbeddingError(
                format!(
                    "CUDA execution provider is not available (provider_present={} cublas_present={}). Install CUDA toolkit/drivers and ensure ORT GPU binaries are present. If you want CPU fallback, set CONTEXT_ALLOW_CPU=1.",
                    report.provider_present, report.cublas_present
                ),
            ));
        }
        Err(err) => {
            return Err(VectorStoreError::EmbeddingError(format!(
                "CUDA execution provider check failed (provider_present={} cublas_present={}): {err}. Run `bash scripts/setup_cuda_deps.sh` (repo checkout) or set ORT_LIB_LOCATION/LD_LIBRARY_PATH. If you want CPU fallback, set CONTEXT_ALLOW_CPU=1.",
                report.provider_present, report.cublas_present
            )));
        }
    }

    Ok(cuda.build())
}

fn zero_tensor(shape: &ndarray::IxDyn, input: &Input) -> Result<DynTensor> {
    let tensor = match &input.input_type {
        ort::value::ValueType::Tensor { ty, .. } => match ty {
            TensorElementType::Int64 => {
                Tensor::from_array(ndarray::Array::<i64, _>::zeros(shape.clone()))
                    .map_err(|e| to_embedding_error(&e))?
                    .upcast()
            }
            TensorElementType::Bool => {
                Tensor::from_array(ndarray::Array::from_elem(shape.clone(), false))
                    .map_err(|e| to_embedding_error(&e))?
                    .upcast()
            }
            TensorElementType::Float32 => {
                Tensor::from_array(ndarray::Array::<f32, _>::zeros(shape.clone()))
                    .map_err(|e| to_embedding_error(&e))?
                    .upcast()
            }
            other => {
                return Err(VectorStoreError::EmbeddingError(format!(
                    "Cannot synthesize zeros for tensor type {other:?} (input {})",
                    input.name
                )))
            }
        },
        other => {
            return Err(VectorStoreError::EmbeddingError(format!(
                "Unsupported input type for zero init: {other:?}"
            )))
        }
    };
    Ok(tensor)
}

/// Embedding model for semantic search running on ONNX Runtime CUDA
pub struct EmbeddingModel {
    backend: EmbeddingBackend,
    dimension: usize,
}

enum EmbeddingBackend {
    Ort(Arc<OrtBackend>),
    Stub(StubBackend),
}

impl EmbeddingModel {
    pub fn new() -> Result<Self> {
        Self::from_env()
    }

    pub fn new_for_model(model_id: &str) -> Result<Self> {
        let mode = EmbeddingMode::from_env()?;
        let id = ModelId::from_raw(model_id);
        Self::from_mode_and_id(mode, &id)
    }

    fn from_env() -> Result<Self> {
        let mode = EmbeddingMode::from_env()?;
        let id = ModelId::from_env();
        Self::from_mode_and_id(mode, &id)
    }

    fn from_mode_and_id(mode: EmbeddingMode, id: &ModelId) -> Result<Self> {
        // Slow path: either wait for in-flight load or become the loader.
        enum Lookup {
            Wait(LoadWaiter<OrtBackend>),
            Load(LoadWaiter<OrtBackend>),
        }

        let spec = id.spec()?;

        if mode == EmbeddingMode::Stub {
            return Ok(Self {
                dimension: spec.dimension,
                backend: EmbeddingBackend::Stub(StubBackend::new(spec.dimension)),
            });
        }

        let dir = model_dir();
        let cache = BACKENDS
            .get_or_init(|| Mutex::new(BackendCache::new(backend_cache_capacity_from_env())));
        if let Ok(mut guard) = cache.lock() {
            if let Some(backend) = guard.get_ready(id) {
                return Ok(Self {
                    dimension: spec.dimension,
                    backend: EmbeddingBackend::Ort(backend),
                });
            }
        }

        let lookup = {
            let mut guard = cache.lock().map_err(|_| {
                VectorStoreError::EmbeddingError("Failed to lock backend cache".into())
            })?;
            match guard.entries.get(id) {
                Some(BackendEntry::Ready(backend)) => {
                    let backend = backend.clone();
                    guard.touch(id);
                    return Ok(Self {
                        dimension: spec.dimension,
                        backend: EmbeddingBackend::Ort(backend),
                    });
                }
                Some(BackendEntry::Loading(waiter)) => Lookup::Wait(waiter.clone()),
                None => Lookup::Load(guard.begin_load(id.clone())),
            }
        };

        let backend = match lookup {
            Lookup::Wait(waiter) => waiter.wait()?,
            Lookup::Load(waiter) => {
                let loaded = OrtBackend::new(&spec, dir.as_path());
                match loaded {
                    Ok(backend) => {
                        let backend = Arc::new(backend);
                        {
                            let mut guard = cache.lock().map_err(|_| {
                                VectorStoreError::EmbeddingError(
                                    "Failed to lock backend cache".into(),
                                )
                            })?;
                            guard.finish_ok(id, backend.clone());
                        }
                        waiter.set_ok(backend.clone());
                        backend
                    }
                    Err(err) => {
                        {
                            let mut guard = cache.lock().map_err(|_| {
                                VectorStoreError::EmbeddingError(
                                    "Failed to lock backend cache".into(),
                                )
                            })?;
                            guard.finish_err(id);
                        }
                        waiter.set_err(format!("{err:#}"));
                        return Err(err);
                    }
                }
            }
        };

        Ok(Self {
            dimension: spec.dimension,
            backend: EmbeddingBackend::Ort(backend),
        })
    }

    #[must_use]
    pub const fn dimension(&self) -> usize {
        self.dimension
    }

    #[cfg(test)]
    pub(crate) fn stub_batch_calls(&self) -> Option<usize> {
        match &self.backend {
            EmbeddingBackend::Stub(stub) => Some(stub.batch_calls()),
            EmbeddingBackend::Ort(_) => None,
        }
    }

    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut embeddings = self.embed_batch(vec![text]).await?;
        embeddings
            .pop()
            .ok_or_else(|| VectorStoreError::EmbeddingError("Empty embedding result".to_string()))
    }

    pub async fn embed_batch(&self, texts: Vec<&str>) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let owned: Vec<String> = texts.into_iter().map(ToString::to_string).collect();
        match &self.backend {
            EmbeddingBackend::Stub(stub) => Ok(stub.embed_batch(&owned)),
            EmbeddingBackend::Ort(backend) => {
                let backend = backend.clone();
                spawn_blocking(move || backend.embed_batch_blocking(&owned))
                    .await
                    .map_err(|e| VectorStoreError::EmbeddingError(format!("Join error: {e}")))?
            }
        }
    }

    #[must_use]
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() {
            return 0.0;
        }

        let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

        if norm_a == 0.0 || norm_b == 0.0 {
            return 0.0;
        }

        dot_product / (norm_a * norm_b)
    }
}

/// Returns the normalized embedding model id for the current process environment.
pub fn current_model_id() -> Result<String> {
    Ok(ModelId::from_env().to_string())
}

/// A multi-model registry that keeps a small LRU of hot ONNX Runtime sessions.
///
/// Design goals:
/// - Explicit model selection (no env mutation needed for multi-model workloads),
/// - Bounded memory usage via an in-memory LRU cache,
/// - Deterministic, stable behavior in the face of concurrent loads (single-flight).
pub struct ModelRegistry {
    model_dir: PathBuf,
    mode: EmbeddingMode,
    specs: HashMap<ModelId, ModelSpec>,
    cache: Mutex<BackendCache<OrtBackend>>,
}

#[derive(Clone, Copy, Debug)]
pub struct EmbedRequest<'a> {
    pub model_id: &'a str,
    pub text: &'a str,
}

impl ModelRegistry {
    pub fn from_env() -> Result<Self> {
        let mode = EmbeddingMode::from_env()?;
        let dir = model_dir();
        let capacity = backend_cache_capacity_from_env();
        Self::new(dir, mode, capacity)
    }

    pub fn new_fast(model_dir: PathBuf, capacity: usize) -> Result<Self> {
        Self::new(model_dir, EmbeddingMode::Fast, capacity)
    }

    pub fn new_stub(model_dir: PathBuf) -> Result<Self> {
        Self::new(model_dir, EmbeddingMode::Stub, 1)
    }

    fn new(model_dir: PathBuf, mode: EmbeddingMode, capacity: usize) -> Result<Self> {
        let specs = load_all_model_specs(&model_dir)?;
        Ok(Self {
            model_dir,
            mode,
            specs,
            cache: Mutex::new(BackendCache::new(capacity)),
        })
    }

    #[must_use]
    pub fn available_models(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.specs.keys().map(ToString::to_string).collect();
        ids.sort();
        ids
    }

    pub fn dimension(&self, model_id: &str) -> Result<usize> {
        let id = ModelId::from_raw(model_id);
        let spec = self.spec_for(&id)?;
        Ok(spec.dimension)
    }

    pub async fn embed(&self, model_id: &str, text: &str) -> Result<Vec<f32>> {
        let mut embeddings = self.embed_batch(model_id, vec![text]).await?;
        embeddings
            .pop()
            .ok_or_else(|| VectorStoreError::EmbeddingError("Empty embedding result".to_string()))
    }

    pub async fn embed_batch(&self, model_id: &str, texts: Vec<&str>) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let id = ModelId::from_raw(model_id);
        let spec = self.spec_for(&id)?;
        let owned: Vec<String> = texts.into_iter().map(ToString::to_string).collect();

        match self.mode {
            EmbeddingMode::Stub => Ok(StubBackend::new(spec.dimension).embed_batch(&owned)),
            EmbeddingMode::Fast => {
                let backend = self.backend_for(&id, &spec)?;
                spawn_blocking(move || backend.embed_batch_blocking(&owned))
                    .await
                    .map_err(|e| VectorStoreError::EmbeddingError(format!("Join error: {e}")))?
            }
        }
    }

    pub async fn embed_batch_multi<'a>(
        &self,
        requests: &[EmbedRequest<'a>],
    ) -> Result<Vec<Vec<f32>>> {
        if requests.is_empty() {
            return Ok(vec![]);
        }

        let mut groups: HashMap<ModelId, Vec<(usize, &'a str)>> = HashMap::new();
        for (idx, req) in requests.iter().enumerate() {
            let id = ModelId::from_raw(req.model_id);
            groups.entry(id).or_default().push((idx, req.text));
        }

        let mut keys: Vec<ModelId> = groups.keys().cloned().collect();
        keys.sort_by(|a, b| a.0.cmp(&b.0));

        let mut out: Vec<Vec<f32>> = vec![Vec::new(); requests.len()];
        for key in keys {
            let entries = groups.remove(&key).unwrap_or_default();
            let texts: Vec<&str> = entries.iter().map(|(_, t)| *t).collect();
            let embeddings = self.embed_batch(&key.to_string(), texts).await?;
            for ((idx, _), embedding) in entries.into_iter().zip(embeddings.into_iter()) {
                if let Some(slot) = out.get_mut(idx) {
                    *slot = embedding;
                }
            }
        }

        Ok(out)
    }

    fn spec_for(&self, id: &ModelId) -> Result<ModelSpec> {
        self.specs.get(id).cloned().ok_or_else(|| {
            let wanted = id.to_string();
            let available = self.available_models().join(", ");
            VectorStoreError::EmbeddingError(format!(
                "Unknown embedding model id '{wanted}'. Available: {available}"
            ))
        })
    }

    fn backend_for(&self, id: &ModelId, spec: &ModelSpec) -> Result<Arc<OrtBackend>> {
        enum Lookup {
            Wait(LoadWaiter<OrtBackend>),
            Load(LoadWaiter<OrtBackend>),
        }

        if let Ok(mut guard) = self.cache.lock() {
            if let Some(backend) = guard.get_ready(id) {
                return Ok(backend);
            }
        }

        let lookup = {
            let mut guard = self.cache.lock().map_err(|_| {
                VectorStoreError::EmbeddingError("Failed to lock model registry cache".into())
            })?;
            match guard.entries.get(id) {
                Some(BackendEntry::Ready(backend)) => {
                    let backend = backend.clone();
                    guard.touch(id);
                    return Ok(backend);
                }
                Some(BackendEntry::Loading(waiter)) => Lookup::Wait(waiter.clone()),
                None => Lookup::Load(guard.begin_load(id.clone())),
            }
        };

        match lookup {
            Lookup::Wait(waiter) => waiter.wait(),
            Lookup::Load(waiter) => {
                let loaded = OrtBackend::new(spec, &self.model_dir);
                match loaded {
                    Ok(backend) => {
                        let backend = Arc::new(backend);
                        {
                            let mut guard = self.cache.lock().map_err(|_| {
                                VectorStoreError::EmbeddingError(
                                    "Failed to lock model registry cache".into(),
                                )
                            })?;
                            guard.finish_ok(id, backend.clone());
                        }
                        waiter.set_ok(backend.clone());
                        Ok(backend)
                    }
                    Err(err) => {
                        {
                            let mut guard = self.cache.lock().map_err(|_| {
                                VectorStoreError::EmbeddingError(
                                    "Failed to lock model registry cache".into(),
                                )
                            })?;
                            guard.finish_err(id);
                        }
                        waiter.set_err(format!("{err:#}"));
                        Err(err)
                    }
                }
            }
        }
    }
}

fn load_all_model_specs(model_dir: &Path) -> Result<HashMap<ModelId, ModelSpec>> {
    let manifest_path = model_dir.join("manifest.json");
    if !manifest_path.exists() {
        // Legacy fallback: keep bge-small working for users who only have the old cache layout.
        let id = ModelId("bge-small".to_string());
        let mut specs = HashMap::new();
        specs.insert(
            id.clone(),
            ModelSpec {
                id,
                onnx_rel_path: PathBuf::from("model.onnx"),
                tokenizer_rel_path: PathBuf::from("tokenizer.json"),
                dimension: 384,
                max_length: 512,
                max_batch: 32,
            },
        );
        return Ok(specs);
    }

    let raw = std::fs::read_to_string(&manifest_path).map_err(|e| {
        VectorStoreError::EmbeddingError(format!(
            "Failed to read models manifest {}: {e}",
            manifest_path.display()
        ))
    })?;
    let manifest: ModelsManifest = serde_json::from_str(&raw).map_err(|e| {
        VectorStoreError::EmbeddingError(format!(
            "Invalid models manifest {}: {e}",
            manifest_path.display()
        ))
    })?;
    if manifest.schema_version != 1 {
        return Err(VectorStoreError::EmbeddingError(format!(
            "Unsupported models manifest schema_version {} (expected 1)",
            manifest.schema_version
        )));
    }

    let mut specs = HashMap::new();
    for model in &manifest.models {
        let id = ModelId::from_raw(&model.id);

        let prefix = format!("{}/", model.id);
        let mut onnx_rel_path: Option<PathBuf> = None;
        let mut tokenizer_rel_path: Option<PathBuf> = None;
        for asset in &model.assets {
            if !asset.path.starts_with(&prefix) {
                continue;
            }
            let rel = asset
                .path
                .strip_prefix(&prefix)
                .unwrap_or(asset.path.as_str());
            let asset_path = std::path::Path::new(asset.path.as_str());
            if onnx_rel_path.is_none()
                && asset_path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("onnx"))
            {
                onnx_rel_path = Some(safe_rel_path_from_manifest(&model.id, rel)?);
            }
            if tokenizer_rel_path.is_none()
                && asset_path
                    .file_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case("tokenizer.json"))
            {
                tokenizer_rel_path = Some(safe_rel_path_from_manifest(&model.id, rel)?);
            }
        }

        specs.insert(
            id.clone(),
            ModelSpec {
                id,
                onnx_rel_path: onnx_rel_path.unwrap_or_else(|| PathBuf::from("model.onnx")),
                tokenizer_rel_path: tokenizer_rel_path
                    .unwrap_or_else(|| PathBuf::from("tokenizer.json")),
                dimension: model.dimension,
                max_length: model.max_length,
                max_batch: model.max_batch,
            },
        );
    }

    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::time::Duration;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("{prefix}_{}_{}", std::process::id(), nanos));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn models_manifest_rejects_path_traversal_assets() {
        let dir = TempDir::new("context_finder_manifest_traversal");
        let manifest = r#"
{
  "schema_version": 1,
  "models": [
    {"id":"m1","dimension":8,"max_length":16,"max_batch":2,"assets":[{"path":"m1/../evil.onnx"}]}
  ]
}
"#;
        std::fs::write(dir.path.join("manifest.json"), manifest).expect("write manifest");
        let Err(err) = load_all_model_specs(&dir.path) else {
            panic!("expected load_all_model_specs to reject traversal paths");
        };
        assert!(
            err.to_string()
                .contains("Invalid models manifest asset path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn models_manifest_rejects_absolute_assets() {
        let dir = TempDir::new("context_finder_manifest_abs");
        let manifest = r#"
{
  "schema_version": 1,
  "models": [
    {"id":"m1","dimension":8,"max_length":16,"max_batch":2,"assets":[{"path":"m1//etc/passwd.onnx"}]}
  ]
}
"#;
        std::fs::write(dir.path.join("manifest.json"), manifest).expect("write manifest");
        let Err(err) = load_all_model_specs(&dir.path) else {
            panic!("expected load_all_model_specs to reject absolute paths");
        };
        assert!(
            err.to_string()
                .contains("Invalid models manifest asset path"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    #[ignore = "Requires ONNX model download and CUDA runtime"]
    async fn test_embed_single() {
        let model = EmbeddingModel::new().unwrap();
        let embedding = model.embed("hello world").await.unwrap();
        assert_eq!(embedding.len(), model.dimension());
    }

    #[tokio::test]
    #[ignore = "Requires ONNX model download and CUDA runtime"]
    async fn test_embed_batch() {
        let model = EmbeddingModel::new().unwrap();
        let texts = vec!["hello world", "foo bar", "test"];
        let embeddings = model.embed_batch(texts).await.unwrap();
        assert_eq!(embeddings.len(), 3);
        for emb in embeddings {
            assert_eq!(emb.len(), model.dimension());
        }
    }

    #[test]
    fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let sim = EmbeddingModel::cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-6);

        let c = vec![1.0, 0.0];
        let d = vec![0.0, 1.0];
        let sim2 = EmbeddingModel::cosine_similarity(&c, &d);
        assert!((sim2 - 0.0).abs() < 1e-6);
    }

    #[test]
    fn backend_cache_evicts_lru_ready() {
        let mut cache: BackendCache<usize> = BackendCache::new(2);

        let id1 = ModelId::from_raw("m1");
        let id2 = ModelId::from_raw("m2");
        let id3 = ModelId::from_raw("m3");

        cache.finish_ok(&id1, Arc::new(1));
        cache.finish_ok(&id2, Arc::new(2));

        // Touch id1 so id2 becomes least-recently used.
        assert_eq!(cache.get_ready(&id1).map(|v| *v), Some(1));

        cache.finish_ok(&id3, Arc::new(3));

        assert!(
            cache.get_ready(&id2).is_none(),
            "expected id2 to be evicted"
        );
        assert!(cache.get_ready(&id1).is_some(), "expected id1 to stay");
        assert!(cache.get_ready(&id3).is_some(), "expected id3 to stay");
    }

    #[test]
    fn backend_cache_single_flight_loads_once() {
        struct DummyBackend {
            value: usize,
        }

        fn backend_for(
            cache: &Arc<Mutex<BackendCache<DummyBackend>>>,
            id: &ModelId,
            loads: &Arc<AtomicUsize>,
        ) -> Arc<DummyBackend> {
            enum Lookup {
                Wait(LoadWaiter<DummyBackend>),
                Load(LoadWaiter<DummyBackend>),
            }

            let lookup = {
                let mut guard = cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match guard.entries.get(id) {
                    Some(BackendEntry::Ready(backend)) => {
                        let backend = backend.clone();
                        guard.touch(id);
                        return backend;
                    }
                    Some(BackendEntry::Loading(waiter)) => Lookup::Wait(waiter.clone()),
                    None => Lookup::Load(guard.begin_load(id.clone())),
                }
            };

            match lookup {
                Lookup::Wait(waiter) => waiter.wait().expect("waiter should complete"),
                Lookup::Load(waiter) => {
                    std::thread::sleep(Duration::from_millis(10));
                    let value = loads.fetch_add(1, Ordering::SeqCst) + 1;
                    let backend = Arc::new(DummyBackend { value });
                    {
                        let mut guard = cache
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        guard.finish_ok(id, backend.clone());
                    }
                    waiter.set_ok(backend.clone());
                    backend
                }
            }
        }

        let cache: Arc<Mutex<BackendCache<DummyBackend>>> =
            Arc::new(Mutex::new(BackendCache::new(2)));
        let loads: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let id = ModelId::from_raw("m1");

        let threads = 8;
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::new();
        for _ in 0..threads {
            let barrier = barrier.clone();
            let id = id.clone();
            let cache = cache.clone();
            let loads = loads.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                backend_for(&cache, &id, &loads)
            }));
        }

        let results: Vec<Arc<DummyBackend>> = handles
            .into_iter()
            .map(|h| h.join().expect("thread should join"))
            .collect();

        assert_eq!(loads.load(Ordering::SeqCst), 1);
        for backend in &results[1..] {
            assert!(
                Arc::ptr_eq(&results[0], backend),
                "all threads should get the same backend instance"
            );
        }
        assert_eq!(results[0].value, 1);
    }

    #[tokio::test]
    async fn model_registry_embed_batch_multi_preserves_request_order() {
        struct TempDir {
            path: PathBuf,
        }

        impl TempDir {
            fn new(prefix: &str) -> Self {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let path =
                    std::env::temp_dir().join(format!("{prefix}_{}_{}", std::process::id(), nanos));
                std::fs::create_dir_all(&path).expect("create temp dir");
                Self { path }
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }

        let dir = TempDir::new("context_finder_models");
        let manifest = r#"
{
  "schema_version": 1,
  "models": [
    {"id":"m1","dimension":8,"max_length":16,"max_batch":2,"assets":[]},
    {"id":"m2","dimension":12,"max_length":16,"max_batch":2,"assets":[]}
  ]
}
"#;
        std::fs::write(dir.path.join("manifest.json"), manifest).expect("write manifest");

        let registry = ModelRegistry::new_stub(dir.path.clone()).expect("create registry");
        let requests = [
            EmbedRequest {
                model_id: "m1",
                text: "hello",
            },
            EmbedRequest {
                model_id: "m2",
                text: "world",
            },
            EmbedRequest {
                model_id: "m1",
                text: "again",
            },
        ];

        let out = registry
            .embed_batch_multi(&requests)
            .await
            .expect("embed batch multi");

        assert_eq!(out.len(), requests.len());
        assert_eq!(out[0].len(), 8);
        assert_eq!(out[1].len(), 12);
        assert_eq!(out[2].len(), 8);

        // Validate stable mapping back to request order.
        assert_eq!(out[0], stub_embed("hello", 8));
        assert_eq!(out[1], stub_embed("world", 12));
        assert_eq!(out[2], stub_embed("again", 8));
    }
}
