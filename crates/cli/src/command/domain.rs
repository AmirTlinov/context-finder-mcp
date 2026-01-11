use anyhow::Result;
use context_protocol::{
    BudgetTruncation, Capabilities, DefaultBudgets, ErrorEnvelope, ToolNextAction,
};
pub use context_search::{ContextPackBudget, ContextPackItem, ContextPackOutput};
pub use context_search::{
    NextAction, NextActionKind, TaskPackItem, TaskPackOutput, TASK_PACK_VERSION,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::PathBuf;

pub const DEFAULT_LIMIT: usize = 10;
pub const DEFAULT_CONTEXT_WINDOW: usize = 20;
pub const BATCH_VERSION: u32 = 1;
pub const MEANING_PACK_VERSION: u32 = 1;
pub const EVIDENCE_FETCH_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
pub struct CommandRequest {
    pub action: CommandAction,
    #[serde(default = "empty_payload")]
    pub payload: Value,
    #[serde(default)]
    pub options: Option<RequestOptions>,
    #[serde(default)]
    pub config: Option<Value>,
}

fn empty_payload() -> Value {
    Value::Object(Default::default())
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandAction {
    Search,
    SearchWithContext,
    ContextPack,
    MeaningPack,
    MeaningFocus,
    TaskPack,
    TextSearch,
    EvidenceFetch,
    Batch,
    Capabilities,
    Index,
    GetContext,
    ListSymbols,
    ConfigRead,
    CompareSearch,
    Map,
    RepoOnboardingPack,
    Eval,
    EvalCompare,
}

impl CommandAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            CommandAction::Search => "search",
            CommandAction::SearchWithContext => "search_with_context",
            CommandAction::ContextPack => "context_pack",
            CommandAction::MeaningPack => "meaning_pack",
            CommandAction::MeaningFocus => "meaning_focus",
            CommandAction::TaskPack => "task_pack",
            CommandAction::TextSearch => "text_search",
            CommandAction::EvidenceFetch => "evidence_fetch",
            CommandAction::Batch => "batch",
            CommandAction::Capabilities => "capabilities",
            CommandAction::Index => "index",
            CommandAction::GetContext => "get_context",
            CommandAction::ListSymbols => "list_symbols",
            CommandAction::ConfigRead => "config_read",
            CommandAction::CompareSearch => "compare_search",
            CommandAction::Map => "map",
            CommandAction::RepoOnboardingPack => "repo_onboarding_pack",
            CommandAction::Eval => "eval",
            CommandAction::EvalCompare => "eval_compare",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct BatchPayload {
    #[serde(default)]
    pub project: Option<PathBuf>,
    #[serde(default)]
    pub max_chars: Option<usize>,
    #[serde(default)]
    pub stop_on_error: bool,
    pub items: Vec<BatchItem>,
}

#[derive(Debug, Deserialize)]
pub struct BatchItem {
    pub id: String,
    pub action: CommandAction,
    #[serde(default = "empty_payload")]
    pub payload: Value,
}

#[derive(Debug, Serialize, Default, Clone)]
pub struct BatchBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<BudgetTruncation>,
}

#[derive(Debug, Serialize, Clone)]
pub struct BatchItemResult {
    pub id: String,
    pub status: CommandStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorEnvelope>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<Hint>,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub meta: ResponseMeta,
}

#[derive(Debug, Serialize, Clone)]
pub struct BatchOutput {
    pub version: u32,
    pub items: Vec<BatchItemResult>,
    pub budget: BatchBudget,
    #[serde(default)]
    pub next_actions: Vec<ToolNextAction>,
}

#[derive(Debug, Serialize)]
pub struct CommandResponse {
    pub status: CommandStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorEnvelope>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<Hint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_actions: Vec<ToolNextAction>,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub meta: ResponseMeta,
}

impl CommandResponse {
    pub fn is_error(&self) -> bool {
        matches!(self.status, CommandStatus::Error)
    }
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Ok,
    Error,
}

#[derive(Debug, Serialize, Clone)]
pub struct Hint {
    #[serde(rename = "type")]
    pub kind: HintKind,
    pub text: String,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum HintKind {
    Info,
    Cache,
    Action,
    Warn,
    Deprecation,
}

#[derive(Debug, Clone)]
pub struct ErrorClassification {
    pub code: String,
    pub hint: Option<String>,
    pub hints: Vec<Hint>,
    pub next_actions: Vec<ToolNextAction>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RequestOptions {
    #[serde(default)]
    pub stale_policy: StalePolicy,
    #[serde(default = "default_max_reindex_ms")]
    pub max_reindex_ms: u64,
    #[serde(default = "default_true")]
    pub allow_filesystem_fallback: bool,
    #[serde(default)]
    pub include_paths: Vec<String>,
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    #[serde(default)]
    pub file_pattern: Option<String>,
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            stale_policy: StalePolicy::default(),
            max_reindex_ms: default_max_reindex_ms(),
            allow_filesystem_fallback: default_true(),
            include_paths: Vec::new(),
            exclude_paths: Vec::new(),
            file_pattern: None,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_max_reindex_ms() -> u64 {
    15000
}

#[derive(Debug, Deserialize, Copy, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum StalePolicy {
    #[default]
    Auto,
    Warn,
    Fail,
}

#[derive(Debug, Serialize, Default, Clone)]
pub struct ResponseMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_cache: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_updated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_mtime_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_nodes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_edges: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_chunks: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_cache_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_last_success_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_last_failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warm_cost_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warm_graph_cache_hit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicates_dropped: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_spans_dropped: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing_load_index_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing_graph_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing_search_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_last_failure_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_failure_reasons: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_p95_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_failure_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_files_per_sec: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_stale_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_pending_events: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_path: Option<String>,
    #[serde(default)]
    pub index_state: Option<context_indexer::IndexState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare_avg_baseline_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare_avg_context_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare_avg_overlap_ratio: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare_avg_related: Option<f32>,
}

pub struct CommandOutcome {
    pub data: Value,
    pub hints: Vec<Hint>,
    pub meta: ResponseMeta,
    pub next_actions: Vec<ToolNextAction>,
}

impl CommandOutcome {
    pub fn from_value<T: Serialize>(value: T) -> Result<Self> {
        Ok(Self {
            data: serde_json::to_value(value)?,
            hints: Vec::new(),
            meta: ResponseMeta::default(),
            next_actions: Vec::new(),
        })
    }
}

pub fn parse_payload<T: DeserializeOwned>(payload: Value) -> Result<T> {
    serde_json::from_value(payload).map_err(Into::into)
}

pub fn merge_configs(base: Option<Value>, overrides: Option<Value>) -> Option<Value> {
    match (base, overrides) {
        (None, None) => None,
        (Some(mut base_value), Some(override_value)) => {
            merge_json(&mut base_value, &override_value);
            Some(base_value)
        }
        (Some(base_value), None) => Some(base_value),
        (None, Some(override_value)) => Some(override_value),
    }
}

fn merge_json(base: &mut Value, overlay: &Value) {
    if let Value::Object(overlay_map) = overlay {
        if !base.is_object() {
            *base = Value::Object(Map::new());
        }

        if let Value::Object(base_map) = base {
            for (key, value) in overlay_map {
                match base_map.get_mut(key) {
                    Some(existing) => merge_json(existing, value),
                    None => {
                        base_map.insert(key.clone(), value.clone());
                    }
                }
            }
        }
    } else {
        *base = overlay.clone();
    }
}

fn config_lookup<'a>(config: &'a Option<Value>, path: &[&str]) -> Option<&'a Value> {
    let mut current = config.as_ref()?;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

pub fn config_string_path(config: &Option<Value>, path: &[&str]) -> Option<String> {
    config_lookup(config, path)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

pub fn config_bool_path(config: &Option<Value>, path: &[&str]) -> Option<bool> {
    config_lookup(config, path).and_then(Value::as_bool)
}

pub fn config_usize_path(config: &Option<Value>, path: &[&str]) -> Option<usize> {
    config_lookup(config, path)
        .and_then(Value::as_u64)
        .map(|raw| raw as usize)
}

pub fn normalize_config(config: Option<Value>) -> Option<Value> {
    config.and_then(|value| if value.is_null() { None } else { Some(value) })
}

#[derive(Debug, Deserialize, Serialize)]
pub struct IndexPayload {
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub full: bool,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub experts: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EvalPayload {
    #[serde(default)]
    pub path: Option<PathBuf>,
    pub dataset: PathBuf,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub cache_mode: Option<EvalCacheMode>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum EvalCacheMode {
    Warm,
    Cold,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvalOutput {
    pub dataset: EvalDatasetMeta,
    pub runs: Vec<EvalRun>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvalDatasetMeta {
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub cases: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvalRun {
    pub profile: String,
    pub models: Vec<String>,
    pub limit: usize,
    pub cache_mode: EvalCacheMode,
    pub summary: EvalSummary,
    pub cases: Vec<EvalCaseResult>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvalRunSummary {
    pub profile: String,
    pub models: Vec<String>,
    pub limit: usize,
    pub cache_mode: EvalCacheMode,
    pub summary: EvalSummary,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvalSummary {
    pub mean_mrr: f64,
    pub mean_recall: f64,
    pub mean_overlap_ratio: f64,
    pub mean_latency_ms: f64,
    pub p50_latency_ms: u64,
    pub p95_latency_ms: u64,
    pub mean_bytes: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvalCaseResult {
    pub id: String,
    pub query: String,
    pub expected_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_symbols: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    pub mrr: f64,
    pub recall: f64,
    pub overlap_ratio: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_rank: Option<usize>,
    pub latency_ms: u64,
    pub bytes: usize,
    pub hits: Vec<EvalHit>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvalHit {
    pub id: String,
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub score: f32,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EvalComparePayload {
    #[serde(default)]
    pub path: Option<PathBuf>,
    pub dataset: PathBuf,
    #[serde(default)]
    pub limit: Option<usize>,
    pub a: EvalCompareConfig,
    pub b: EvalCompareConfig,
    #[serde(default)]
    pub cache_mode: Option<EvalCacheMode>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EvalCompareConfig {
    pub profile: String,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvalCompareOutput {
    pub dataset: EvalDatasetMeta,
    pub cache_mode: EvalCacheMode,
    pub a: EvalRunSummary,
    pub b: EvalRunSummary,
    pub summary: EvalCompareSummary,
    pub cases: Vec<EvalCompareCase>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvalCompareSummary {
    pub delta_mean_mrr: f64,
    pub delta_mean_recall: f64,
    pub delta_mean_overlap_ratio: f64,
    pub delta_mean_latency_ms: f64,
    pub delta_p95_latency_ms: i64,
    pub delta_mean_bytes: f64,
    pub a_wins: usize,
    pub b_wins: usize,
    pub ties: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvalCompareCase {
    pub id: String,
    pub query: String,
    pub expected_paths: Vec<String>,
    pub a_mrr: f64,
    pub b_mrr: f64,
    pub delta_mrr: f64,
    pub a_recall: f64,
    pub b_recall: f64,
    pub delta_recall: f64,
    pub a_overlap_ratio: f64,
    pub b_overlap_ratio: f64,
    pub delta_overlap_ratio: f64,
    pub a_latency_ms: u64,
    pub b_latency_ms: u64,
    pub delta_latency_ms: i64,
    pub a_bytes: usize,
    pub b_bytes: usize,
    pub delta_bytes: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a_first_rank: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b_first_rank: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SearchPayload {
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub project: Option<PathBuf>,
    #[serde(default)]
    pub trace: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchWithContextPayload {
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub project: Option<PathBuf>,
    #[serde(default)]
    pub strategy: Option<SearchStrategy>,
    #[serde(default)]
    pub show_graph: Option<bool>,
    #[serde(default)]
    pub trace: Option<bool>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub reuse_graph: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TextSearchPayload {
    pub pattern: String,
    #[serde(default)]
    pub max_results: Option<usize>,
    #[serde(default)]
    pub case_sensitive: Option<bool>,
    #[serde(default)]
    pub whole_word: Option<bool>,
    #[serde(default)]
    pub project: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TextSearchMatch {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub text: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TextSearchOutput {
    pub pattern: String,
    pub source: String,
    pub scanned_files: usize,
    pub matched_files: usize,
    pub skipped_large_files: usize,
    pub returned: usize,
    pub truncated: bool,
    pub matches: Vec<TextSearchMatch>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ContextPackPayload {
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub project: Option<PathBuf>,
    #[serde(default)]
    pub strategy: Option<SearchStrategy>,
    #[serde(default)]
    pub max_chars: Option<usize>,
    #[serde(default)]
    pub max_related_per_primary: Option<usize>,
    /// Prefer code results over markdown docs (implementation-first).
    #[serde(default)]
    pub prefer_code: Option<bool>,
    /// Whether markdown docs (e.g. *.md) may be included in the pack (default: true).
    #[serde(default)]
    pub include_docs: Option<bool>,
    /// Related context mode: \"explore\" (default) or \"focus\" (query-gated).
    #[serde(default)]
    pub related_mode: Option<String>,
    #[serde(default)]
    pub trace: Option<bool>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub reuse_graph: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MeaningPackPayload {
    /// Natural-language query describing what to orient on.
    pub query: String,
    #[serde(default)]
    pub project: Option<PathBuf>,
    /// Maximum UTF-8 characters for the output payload.
    #[serde(default)]
    pub max_chars: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MeaningFocusPayload {
    /// Repo-relative file or directory path to focus on.
    pub focus: String,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub project: Option<PathBuf>,
    /// Maximum UTF-8 characters for the output payload.
    #[serde(default)]
    pub max_chars: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TaskPackPayload {
    pub intent: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub project: Option<PathBuf>,
    #[serde(default)]
    pub strategy: Option<SearchStrategy>,
    #[serde(default)]
    pub max_chars: Option<usize>,
    #[serde(default)]
    pub max_related_per_primary: Option<usize>,
    /// Prefer code results over markdown docs (implementation-first).
    #[serde(default)]
    pub prefer_code: Option<bool>,
    /// Whether markdown docs (e.g. *.md) may be included in the pack (default: true).
    #[serde(default)]
    pub include_docs: Option<bool>,
    /// Related context mode: \"explore\" (default) or \"focus\" (query-gated).
    #[serde(default)]
    pub related_mode: Option<String>,
    #[serde(default)]
    pub trace: Option<bool>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub reuse_graph: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct CompareSearchPayload {
    #[serde(default)]
    pub queries: Vec<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub project: Option<PathBuf>,
    #[serde(default)]
    pub strategy: Option<SearchStrategy>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub reuse_graph: Option<bool>,
    #[serde(default)]
    pub show_graph: Option<bool>,
    #[serde(default)]
    pub invalidate_cache: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SearchStrategy {
    Direct,
    #[default]
    Extended,
    Deep,
}

impl SearchStrategy {
    pub const fn to_assembly(self) -> context_graph::AssemblyStrategy {
        match self {
            SearchStrategy::Direct => context_graph::AssemblyStrategy::Direct,
            SearchStrategy::Extended => context_graph::AssemblyStrategy::Extended,
            SearchStrategy::Deep => context_graph::AssemblyStrategy::Deep,
        }
    }

    pub fn from_name(value: &str) -> Option<Self> {
        match value.to_lowercase().as_str() {
            "direct" => Some(SearchStrategy::Direct),
            "extended" => Some(SearchStrategy::Extended),
            "deep" => Some(SearchStrategy::Deep),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            SearchStrategy::Direct => "direct",
            SearchStrategy::Extended => "extended",
            SearchStrategy::Deep => "deep",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct GetContextPayload {
    pub file: String,
    pub line: usize,
    #[serde(default = "default_window")]
    pub window: usize,
    #[serde(default)]
    pub project: Option<PathBuf>,
}

fn default_window() -> usize {
    DEFAULT_CONTEXT_WINDOW
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EvidencePointer {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(default)]
    pub byte_start: Option<usize>,
    #[serde(default)]
    pub byte_end: Option<usize>,
    #[serde(default)]
    pub source_hash: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvidenceFetchPayload {
    #[serde(default)]
    pub project: Option<PathBuf>,
    pub items: Vec<EvidencePointer>,
    /// Maximum UTF-8 characters for the entire response payload.
    #[serde(default)]
    pub max_chars: Option<usize>,
    /// Maximum lines per evidence item (soft cap).
    #[serde(default)]
    pub max_lines: Option<usize>,
    /// When true, treat source_hash mismatches as an error.
    #[serde(default)]
    pub strict_hash: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MeaningPackBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<BudgetTruncation>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MeaningPackOutput {
    pub version: u32,
    pub query: String,
    pub format: String,
    pub pack: String,
    pub budget: MeaningPackBudget,
    #[serde(default)]
    pub next_actions: Vec<ToolNextAction>,
    #[serde(default)]
    pub meta: context_indexer::ToolMeta,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvidenceFetchBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<BudgetTruncation>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvidenceFetchItem {
    pub evidence: EvidencePointer,
    pub content: String,
    pub truncated: bool,
    #[serde(default)]
    pub stale: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvidenceFetchOutput {
    pub version: u32,
    pub items: Vec<EvidenceFetchItem>,
    pub budget: EvidenceFetchBudget,
    #[serde(default)]
    pub next_actions: Vec<ToolNextAction>,
    #[serde(default)]
    pub meta: context_indexer::ToolMeta,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ListSymbolsPayload {
    pub file: String,
    #[serde(default)]
    pub project: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ConfigReadPayload {
    #[serde(default)]
    pub project: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
pub struct IndexResponse {
    pub stats: context_indexer::IndexStats,
}

#[derive(Serialize)]
pub struct ConfigReadResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
}

#[allow(dead_code)]
pub type CapabilitiesResponse = Capabilities;

#[derive(Serialize, Deserialize)]
pub struct SearchOutput {
    pub query: String,
    pub results: Vec<SearchResultOutput>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ComparisonOutput {
    pub project: String,
    pub limit: usize,
    pub strategy: String,
    pub reuse_graph: bool,
    pub queries: Vec<QueryComparison>,
    pub summary: ComparisonSummary,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct QueryComparison {
    pub query: String,
    pub limit: usize,
    pub baseline_duration_ms: u64,
    pub context_duration_ms: u64,
    pub overlap: usize,
    pub overlap_ratio: f32,
    pub context_related: usize,
    pub baseline: Vec<SearchResultOutput>,
    pub context: Vec<SearchResultOutput>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ComparisonSummary {
    pub avg_baseline_ms: f32,
    pub avg_context_ms: f32,
    pub avg_overlap_ratio: f32,
    pub avg_related_chunks: f32,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SearchResultOutput {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub symbol: Option<String>,
    #[serde(rename = "type")]
    pub chunk_type: Option<String>,
    pub score: f32,
    pub content: String,
    pub context: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related: Option<Vec<RelatedCodeOutput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph: Option<Vec<RelationshipOutput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RelatedCodeOutput {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub symbol: Option<String>,
    pub relationship: Vec<String>,
    pub distance: usize,
    pub relevance: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RelationshipOutput {
    pub from: String,
    pub to: String,
    pub relationship: String,
}

#[derive(Serialize)]
pub struct ContextOutput {
    pub file: String,
    pub line: usize,
    pub symbol: Option<String>,
    #[serde(rename = "type")]
    pub chunk_type: Option<String>,
    pub parent: Option<String>,
    pub imports: Vec<String>,
    pub content: String,
    pub window: WindowOutput,
}

#[derive(Serialize)]
pub struct WindowOutput {
    pub before: String,
    pub after: String,
}

#[derive(Serialize, Deserialize)]
pub struct SymbolsOutput {
    /// File name (for single-file mode) or pattern used
    pub file: String,
    /// All symbols found
    pub symbols: Vec<SymbolInfo>,
    /// Number of files processed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_count: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SymbolInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub symbol_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub line: usize,
    /// File path (for multi-file mode)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MapPayload {
    #[serde(default)]
    pub project: Option<PathBuf>,
    #[serde(default = "map_default_depth")]
    pub depth: usize,
    #[serde(default)]
    pub limit: Option<usize>,
}

fn map_default_depth() -> usize {
    2
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MapOutput {
    pub nodes: Vec<MapNode>,
    pub total_files: usize,
    pub total_chunks: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_chunks_pct: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_files_pct: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_lines_pct: Option<f32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MapNode {
    pub path: String,
    pub files: usize,
    pub chunks: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_chunks_pct: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_files_pct: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_lines_pct: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_symbols: Option<Vec<SymbolInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_symbol_coverage: Option<f32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RepoOnboardingPackPayload {
    #[serde(default, alias = "path")]
    pub project: Option<PathBuf>,
    #[serde(default)]
    pub map_depth: Option<usize>,
    #[serde(default)]
    pub map_limit: Option<usize>,
    #[serde(default)]
    pub doc_paths: Option<Vec<String>>,
    #[serde(default)]
    pub docs_limit: Option<usize>,
    #[serde(default)]
    pub doc_max_lines: Option<usize>,
    #[serde(default)]
    pub doc_max_chars: Option<usize>,
    #[serde(default)]
    pub max_chars: Option<usize>,
    #[serde(default)]
    pub auto_index: Option<bool>,
    #[serde(default)]
    pub auto_index_budget_ms: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepoOnboardingDocsReason {
    DocsLimitZero,
    NoDocCandidates,
    DocsNotFound,
    MaxChars,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RepoOnboardingPackBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<BudgetTruncation>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RepoOnboardingDocSlice {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub returned_lines: usize,
    pub used_chars: usize,
    pub max_lines: usize,
    pub max_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<BudgetTruncation>,
    pub file_size_bytes: u64,
    pub file_mtime_ms: u64,
    pub content_sha256: String,
    pub content: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RepoOnboardingPackOutput {
    pub version: u32,
    pub root: String,
    pub map: MapOutput,
    pub docs: Vec<RepoOnboardingDocSlice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs_reason: Option<RepoOnboardingDocsReason>,
    pub next_actions: Vec<ToolNextAction>,
    pub budget: RepoOnboardingPackBudget,
}

pub fn classify_error(
    message: &str,
    action: Option<CommandAction>,
    payload: Option<&Value>,
) -> ErrorClassification {
    let mut hints = Vec::new();
    let mut next_actions = Vec::new();
    let mut code = "internal".to_string();
    let mut hint = None;

    if message.contains("Index not found") {
        code = "index_missing".to_string();
        hint = Some(
            "Index missing — run action=index with payload.path set to the project root."
                .to_string(),
        );
        hints.push(Hint {
            kind: HintKind::Action,
            text: hint.clone().expect("hint"),
        });
        let path = extract_project_path(payload).unwrap_or_else(|| ".".to_string());
        if action != Some(CommandAction::Index) {
            next_actions.push(ToolNextAction {
                tool: CommandAction::Index.as_str().to_string(),
                args: json!({ "path": path }),
                reason: "Build the semantic index (required for search/context/context_pack)."
                    .to_string(),
            });
        }
    }

    if message.contains("Failed to load vector store") {
        code = "index_corrupt".to_string();
        hints.push(Hint {
            kind: HintKind::Action,
            text: "Index file looks corrupted — delete .agents/mcp/context/.context/indexes/<model_id>/index.json and rerun the index action."
                .to_string(),
        });
    }

    if message.contains("Failed to read metadata") {
        code = "filesystem_error".to_string();
        hints.push(Hint {
            kind: HintKind::Warn,
            text: "Filesystem metadata unavailable — check permissions or run from inside the project directory.".to_string(),
        });
    }

    if message.to_lowercase().contains("graph language") {
        code = "invalid_request".to_string();
        hints.push(Hint {
            kind: HintKind::Action,
            text: "Specify graph_language in config or payload to enable context graph assembly."
                .to_string(),
        });
    }

    if message.to_lowercase().contains("config") {
        code = "config_error".to_string();
        hints.push(Hint {
            kind: HintKind::Warn,
            text: "Config issue detected — verify .agents/mcp/context/.context/config.json or remove it."
                .to_string(),
        });
    }

    if message.contains("Project path does not exist") {
        code = "invalid_request".to_string();
        hints.push(Hint {
            kind: HintKind::Action,
            text: "Check payload.path/project or run from the repository root.".to_string(),
        });
    }

    if message.contains("File not found") {
        code = "invalid_request".to_string();
        hints.push(Hint {
            kind: HintKind::Action,
            text: "Verify the 'file' value is relative to project root and exists on disk."
                .to_string(),
        });
    }

    if message.contains("max_chars too small") {
        code = "invalid_request".to_string();
        hints.push(Hint {
            kind: HintKind::Action,
            text: "Increase max_chars; current budget is too small for the response envelope."
                .to_string(),
        });
        let min_chars = parse_min_chars(message);
        if let Some(action) = action {
            if let Some(mut max_chars) = default_max_chars_for_action(action) {
                if let Some(min_chars) = min_chars {
                    max_chars = max_chars.max(min_chars);
                }
                let args = build_retry_args(payload, max_chars);
                next_actions.push(ToolNextAction {
                    tool: action.as_str().to_string(),
                    args,
                    reason: format!("Retry {} with max_chars={}.", action.as_str(), max_chars),
                });
            }
        }
    }

    if message.contains("budget exceeded") {
        code = "invalid_request".to_string();
        hints.push(Hint {
            kind: HintKind::Action,
            text: "Increase max_chars; response budget was exceeded.".to_string(),
        });
        if let Some(action) = action {
            if let Some(max_chars) = default_max_chars_for_action(action) {
                let args = build_retry_args(payload, max_chars);
                next_actions.push(ToolNextAction {
                    tool: action.as_str().to_string(),
                    args,
                    reason: format!("Retry {} with max_chars={}.", action.as_str(), max_chars),
                });
            }
        }
    }

    if hint.is_none() {
        hint = hints.first().map(|h| h.text.clone());
    }

    ErrorClassification {
        code,
        hint,
        hints,
        next_actions,
    }
}

fn extract_project_path(payload: Option<&Value>) -> Option<String> {
    let payload = payload?;
    payload
        .get("project")
        .or_else(|| payload.get("path"))
        .and_then(Value::as_str)
        .map(|value| value.to_string())
}

fn parse_min_chars(message: &str) -> Option<usize> {
    let (_, tail) = message.split_once("min_chars=")?;
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn build_retry_args(payload: Option<&Value>, max_chars: usize) -> Value {
    let mut map = payload
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    map.insert("max_chars".to_string(), Value::Number(max_chars.into()));
    Value::Object(map)
}

fn default_max_chars_for_action(action: CommandAction) -> Option<usize> {
    let budgets = DefaultBudgets::default();
    match action {
        CommandAction::ContextPack | CommandAction::TaskPack => {
            Some(budgets.context_pack_max_chars)
        }
        CommandAction::RepoOnboardingPack => Some(budgets.repo_onboarding_pack_max_chars),
        CommandAction::Batch => Some(budgets.batch_max_chars),
        _ => None,
    }
}
