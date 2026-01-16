use anyhow::Result;
use context_indexer::FileScanner;
use context_protocol::{enforce_max_chars, BudgetTruncation};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use crate::common::{
    artifact_scope_rank, build_ev_file_index, classify_boundaries, classify_files, contract_kind,
    detect_brokers, detect_channel_mentions, directory_key, extract_asyncapi_flows,
    hash_and_count_lines, infer_actor_by_path, infer_flow_actor, is_artifact_scope,
    is_binary_blob_path, is_ci_config_candidate, is_code_file, is_contract_candidate,
    is_dataset_like_path, json_string, read_file_prefix_utf8, shrink_pack, AnchorKind,
    BoundaryCandidate, BoundaryKind, BrokerCandidate, CognitivePack, EvidenceItem, EvidenceKind,
    FlowEdge,
};
use crate::model::{MeaningPackBudget, MeaningPackRequest, MeaningPackResult};
use crate::paths::normalize_relative_path;
use crate::secrets::is_potential_secret_path;

const VERSION: u32 = 1;
const DEFAULT_MAX_CHARS: usize = 2_000;
const MIN_MAX_CHARS: usize = 800;
const MAX_MAX_CHARS: usize = 500_000;
const DEFAULT_MAP_DEPTH: usize = 2;
const DEFAULT_MAP_LIMIT: usize = 12;
const DEFAULT_MAX_EVIDENCE: usize = 12;
const DEFAULT_MAX_ANCHORS: usize = 7;
const DEFAULT_MAX_BOUNDARIES: usize = 12;
const DEFAULT_MAX_ENTRYPOINTS: usize = 8;
const DEFAULT_MAX_CONTRACTS: usize = 8;
const DEFAULT_MAX_FLOWS: usize = 12;
const DEFAULT_MAX_BROKERS: usize = 6;
const DEFAULT_EVIDENCE_END_LINE: usize = 120;

#[derive(Debug, Clone, Copy)]
struct QueryHints {
    wants_entrypoints: bool,
    wants_contracts: bool,
    wants_brokers: bool,
    wants_infra: bool,
    wants_ci: bool,
    wants_artifacts: bool,
    wants_experiments: bool,
}

impl QueryHints {
    fn from_query(query: &str) -> Self {
        // Keep this deterministic and cheap: we only use query hints to prioritize sections under
        // tight budgets. We intentionally support both English and Russian keywords because the
        // operator language in this environment is often RU, while code/docs are EN.
        let lc = query.to_lowercase();
        let contains_any = |needles: &[&str]| needles.iter().any(|n| lc.contains(n));

        let wants_entrypoints = contains_any(&[
            // EN
            "entrypoint",
            "entrypoints",
            "main",
            "cli",
            // RU
            "точка входа",
            "точки входа",
            "входная точка",
            "входные точки",
            "запуск",
            "старт",
        ]);
        let wants_contracts = contains_any(&[
            // EN
            "contract",
            "contracts",
            "openapi",
            "asyncapi",
            "schema",
            "proto",
            "grpc",
            "api",
            // RU
            "контракт",
            "контракты",
            "схема",
            "схемы",
            "спека",
            "спеки",
            "спецификац",
            "прото",
            "протокол",
            "апи",
        ]);
        let wants_brokers = contains_any(&[
            // EN
            "broker",
            "brokers",
            "kafka",
            "nats",
            "amqp",
            "rabbit",
            "rabbitmq",
            "redis",
            // RU
            "брокер",
            "брокеры",
            "кафка",
            "натс",
            "очеред",
            "шина",
        ]);
        let wants_infra = contains_any(&[
            // EN
            "infra",
            "config",
            "configs",
            "env",
            "environment",
            "settings",
            "deploy",
            "k8s",
            "kubernetes",
            "helm",
            "terraform",
            "gitops",
            "boundary",
            // RU
            "инфра",
            "конфиг",
            "конфиги",
            "переменн",
            "окружен",
            "настройк",
            "деплой",
            "развер",
        ]);
        let wants_ci = contains_any(&[
            // EN
            "ci",
            "workflow",
            "workflows",
            "github actions",
            "pipeline",
            "pipelines",
            // RU
            "ci",
            "пайплайн",
            "гейт",
            "гейты",
            "экшн",
            "actions",
            "воркфло",
        ]);
        let wants_artifacts = contains_any(&[
            // EN
            "artifact",
            "artifacts",
            "output",
            "outputs",
            "result",
            "results",
            "checkpoint",
            "checkpoints",
            // RU
            "артефакт",
            "артефакты",
            "вывод",
            "результат",
            "результаты",
            "чекпоинт",
            "чекпоинты",
        ]);
        let wants_experiments = contains_any(&[
            // EN
            "experiment",
            "experiments",
            "baseline",
            "baselines",
            "bench",
            "benchmark",
            "eval",
            "evaluation",
            "research",
            "analysis",
            "notebook",
            // RU
            "эксперимент",
            "эксперименты",
            "бейслайн",
            "бенч",
            "оценк",
            "исслед",
            "анализ",
            "ноутбук",
        ]);

        Self {
            wants_entrypoints,
            wants_contracts,
            wants_brokers,
            wants_infra,
            wants_ci,
            wants_artifacts,
            wants_experiments,
        }
    }
}

#[derive(Debug, Clone)]
struct AnchorCandidate {
    kind: AnchorKind,
    label: String,
    file: String,
    confidence: f32,
}

#[derive(Debug, Clone)]
struct EmittedAnchor {
    kind: AnchorKind,
    label: String,
    file: String,
    confidence: f32,
    ev_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PipelineStepKind {
    Setup,
    Build,
    Run,
    Test,
    Eval,
    Lint,
    Format,
}

impl PipelineStepKind {
    fn as_str(self) -> &'static str {
        match self {
            PipelineStepKind::Setup => "setup",
            PipelineStepKind::Build => "build",
            PipelineStepKind::Run => "run",
            PipelineStepKind::Test => "test",
            PipelineStepKind::Eval => "eval",
            PipelineStepKind::Lint => "lint",
            PipelineStepKind::Format => "format",
        }
    }
}

fn pipeline_step_rank(kind: PipelineStepKind) -> usize {
    match kind {
        PipelineStepKind::Setup => 0,
        PipelineStepKind::Build => 1,
        PipelineStepKind::Run => 2,
        PipelineStepKind::Test => 3,
        PipelineStepKind::Eval => 4,
        PipelineStepKind::Lint => 5,
        PipelineStepKind::Format => 6,
    }
}

#[derive(Debug, Clone)]
struct PipelineStepCandidate {
    kind: PipelineStepKind,
    label: String,
    confidence: f32,
}

#[derive(Debug, Clone)]
struct EmittedPipelineStep {
    kind: PipelineStepKind,
    label: String,
    confidence: f32,
    ev_id: String,
}

#[derive(Debug, Clone)]
struct EmittedArea {
    kind: &'static str,
    label: String,
    path: String,
    confidence: f32,
    ev_id: Option<String>,
}

#[derive(Default)]
struct RepoSignals {
    total_files: usize,
    code_files: usize,
    contract_candidates: usize,
    ci_files: usize,
    dataset_like_files: usize,
    dataset_like_bytes: u64,
    binary_blob_files: usize,
    manifest_files: usize,
    manifest_scopes: HashSet<String>,
}

impl RepoSignals {
    fn is_dataset_heavy(&self) -> bool {
        if self.total_files < 50 {
            return false;
        }
        let data_ratio = self.dataset_like_files as f32 / self.total_files as f32;
        // Fail-soft heuristic: either “lots of dataset files” or “dataset bytes dominate”.
        data_ratio >= 0.25
            || (self.dataset_like_files >= 100 && self.dataset_like_files > self.code_files * 2)
            || self.dataset_like_bytes >= 256 * 1024 * 1024
    }

    fn is_monorepo(&self) -> bool {
        let scopes = self
            .manifest_scopes
            .iter()
            .filter(|s| s.as_str() != ".")
            .count();
        self.manifest_files >= 4 || scopes >= 2
    }
}

fn is_manifest_candidate(file_lc: &str) -> bool {
    let lc = file_lc.trim();
    if lc.is_empty() {
        return false;
    }
    let basename = lc.rsplit('/').next().unwrap_or(lc);
    matches!(
        basename,
        "cargo.toml"
            | "cargo.lock"
            | "package.json"
            | "go.mod"
            | "pyproject.toml"
            | "requirements.txt"
            | "setup.py"
            | "setup.cfg"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "cmakelists.txt"
    )
}

fn should_suppress_from_map(file_lc: &str, bytes: u64) -> bool {
    // Never suppress code/entrypoints/contracts/CI: even if large, they are part of “meaning”.
    if is_code_file(file_lc) || is_contract_candidate(file_lc) || is_ci_config_candidate(file_lc) {
        return false;
    }

    let lc = file_lc.trim();
    if lc.is_empty() {
        return true;
    }

    // Obvious heavy blobs that add noise to structure maps.
    if is_dataset_like_path(lc) || is_binary_blob_path(lc) {
        return true;
    }

    // Logs/dumps are rarely part of repo “meaning” (and are often huge).
    if lc.ends_with(".log")
        || lc.ends_with(".trace")
        || lc.ends_with(".out")
        || lc.ends_with(".dump")
        || lc.ends_with(".tmp")
    {
        return true;
    }

    // Common generated/build output scopes (best-effort; gitignore may already hide them).
    for scope in [
        "dist/",
        "build/",
        "out/",
        ".cache/",
        ".venv/",
        ".worktrees/",
        ".mypy_cache/",
        ".pytest_cache/",
        ".ruff_cache/",
    ] {
        if lc.starts_with(scope) || lc.contains(&format!("/{scope}")) {
            return true;
        }
    }

    // Heuristic: very large JSON/YAML/TOML are often datasets or machine outputs.
    if bytes >= 1_000_000
        && (lc.ends_with(".json")
            || lc.ends_with(".yaml")
            || lc.ends_with(".yml")
            || lc.ends_with(".toml")
            || lc.ends_with(".txt"))
    {
        return true;
    }

    false
}

pub async fn meaning_pack(
    root: &Path,
    root_display: &str,
    request: &MeaningPackRequest,
) -> Result<MeaningPackResult> {
    let max_chars = request
        .max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);
    let hints = QueryHints::from_query(&request.query);
    let tight_budget = max_chars <= 2_200;

    // v0: facts-only map derived from filesystem paths (gitignore-aware), no full-file parsing.
    let scanner = FileScanner::new(root);
    let mut files: Vec<String> = Vec::new();
    let mut sizes: HashMap<String, u64> = HashMap::new();
    let mut signals = RepoSignals::default();
    for abs in scanner.scan() {
        let Some(rel) = normalize_relative_path(root, &abs) else {
            continue;
        };
        if is_potential_secret_path(&rel) {
            continue;
        }
        let bytes = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
        sizes
            .entry(rel.clone())
            .and_modify(|existing| *existing = (*existing).max(bytes))
            .or_insert(bytes);

        let lc = rel.to_ascii_lowercase();
        signals.total_files += 1;
        if is_code_file(&lc) {
            signals.code_files += 1;
        }
        if is_contract_candidate(&lc) {
            signals.contract_candidates += 1;
        }
        if is_ci_config_candidate(&lc) {
            signals.ci_files += 1;
        }
        if is_dataset_like_path(&lc) {
            signals.dataset_like_files += 1;
            signals.dataset_like_bytes = signals.dataset_like_bytes.saturating_add(bytes);
        }
        if is_binary_blob_path(&lc) {
            signals.binary_blob_files += 1;
        }
        if is_manifest_candidate(&lc) {
            signals.manifest_files += 1;
            // Use depth=2 so common monorepo layouts (`crates/x`, `packages/y`) separate naturally.
            signals.manifest_scopes.insert(directory_key(&rel, 2));
        }

        files.push(rel);
    }
    // Meaning mode cares about some \"noise\" files that we intentionally skip for indexing/watcher
    // workflows (e.g. docker-compose). Add them back explicitly so broker/boundary extraction can
    // use them as evidence anchors.
    for rel in [
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
        "Dockerfile",
        "dockerfile",
        "Makefile",
        "makefile",
        "Justfile",
        "JUSTFILE",
        "justfile",
    ] {
        if root.join(rel).is_file() && !files.iter().any(|existing| existing == rel) {
            files.push(rel.to_string());
            let abs = root.join(rel);
            let bytes = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
            sizes
                .entry(rel.to_string())
                .and_modify(|existing| *existing = (*existing).max(bytes))
                .or_insert(bytes);
        }
    }
    files.sort();
    files.dedup();

    // Dynamic defaults (signal-driven): allow a more useful map without requiring explicit
    // map_depth/map_limit tuning by the caller.
    let mut map_depth = request.map_depth.unwrap_or(DEFAULT_MAP_DEPTH);
    let mut map_limit = request.map_limit.unwrap_or(DEFAULT_MAP_LIMIT);
    if request.map_limit.is_none() && signals.is_dataset_heavy() {
        map_limit = 10;
    }
    if request.map_limit.is_none() && signals.is_monorepo() {
        map_limit = map_limit.max(16);
    }
    map_depth = map_depth.clamp(1, 4);
    map_limit = map_limit.clamp(1, 200);

    let mut dir_files: HashMap<String, usize> = HashMap::new();
    let mut dir_files_with_artifacts: HashMap<String, usize> = HashMap::new();
    for rel in &files {
        let lc = rel.to_ascii_lowercase();
        let bytes = sizes.get(rel).copied().unwrap_or(0);
        let key = directory_key(rel, map_depth);
        *dir_files_with_artifacts.entry(key.clone()).or_insert(0) += 1;
        if is_artifact_scope(&lc) {
            continue;
        }
        if should_suppress_from_map(&lc, bytes) {
            continue;
        }
        *dir_files.entry(key).or_insert(0) += 1;
    }

    let mut map_rows = dir_files.into_iter().collect::<Vec<_>>();
    map_rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if map_rows.is_empty() {
        // Fail-soft: if everything is under an artifact store (rare but possible), fall back to
        // the unsuppressed counts so the agent still sees *some* structure.
        map_rows = dir_files_with_artifacts.into_iter().collect::<Vec<_>>();
        map_rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    }

    let (entrypoints, contracts) = classify_files(&files);
    let mut boundaries_full = classify_boundaries(&files, &entrypoints, &contracts);
    augment_k8s_manifest_boundaries(root, &files, &mut boundaries_full).await;
    let artifact_store_file = best_artifact_store_evidence_file(&files);
    // Budget-aware: keep anchors stable and actionable under tight budgets by reducing
    // lower-signal sections (boundaries/entrypoints/flows) unless the query asks for them.
    let max_anchors = if tight_budget { 5 } else { DEFAULT_MAX_ANCHORS };
    let anchors = select_repo_anchors(
        &files,
        &entrypoints,
        &contracts,
        &boundaries_full,
        artifact_store_file.as_deref(),
        &hints,
        max_anchors,
    );
    let include_entrypoints = !tight_budget || hints.wants_entrypoints;
    // Signal-driven boundary inclusion: keep “external” boundaries (HTTP/CLI/events/DB) even when
    // the query is a broad onboarding prompt that doesn't explicitly say “infra/boundary”.
    // This stays low-noise for library-only repos where we only have config/env candidates.
    let has_external_boundaries = boundaries_full.iter().any(|b| {
        matches!(
            b.kind,
            BoundaryKind::Http | BoundaryKind::Cli | BoundaryKind::Event | BoundaryKind::Db
        )
    });
    let include_boundaries =
        !tight_budget || hints.wants_infra || hints.wants_brokers || has_external_boundaries;
    if !include_boundaries {
        boundaries_full.clear();
    }
    boundaries_full.truncate(DEFAULT_MAX_BOUNDARIES);
    let boundaries = boundaries_full;

    let flows = extract_asyncapi_flows(root, &contracts).await;

    let channels = flows.iter().map(|f| f.channel.clone()).collect::<Vec<_>>();
    let channel_mentions = detect_channel_mentions(root, &files, &channels).await;

    // Event flows are high-signal: if AsyncAPI is present, we keep flows even under tight budgets.
    let include_flows = !flows.is_empty();
    let include_brokers =
        !tight_budget || hints.wants_brokers || hints.wants_infra || !flows.is_empty();
    let flows = if include_flows { flows } else { Vec::new() };
    let brokers = if include_brokers {
        detect_brokers(root, &files, &flows).await
    } else {
        Vec::new()
    };

    let evidence_entrypoints: &[String] = if include_entrypoints {
        &entrypoints
    } else {
        &[]
    };
    let evidence_contracts: &[String] = if hints.wants_contracts {
        &contracts
    } else {
        &[]
    };
    let evidence_boundaries: &[BoundaryCandidate] =
        if include_boundaries { &boundaries } else { &[] };

    let evidence = collect_evidence(
        root,
        &anchors,
        evidence_entrypoints,
        evidence_contracts,
        evidence_boundaries,
        &flows,
        &brokers,
    )
    .await;
    let ev_file_index = build_ev_file_index(&evidence);

    // Evidence-driven map ranking: prefer directories that contain “sources of truth” over those
    // that merely have many files (dataset-heavy repos, vendored trees, etc.).
    let mut dir_scores: HashMap<String, i32> = HashMap::new();
    for ev in &evidence {
        let dir = directory_key(&ev.file, map_depth);
        let weight = match &ev.kind {
            EvidenceKind::Anchor(AnchorKind::Canon) => 100,
            EvidenceKind::Anchor(AnchorKind::HowTo) => 95,
            EvidenceKind::Anchor(AnchorKind::Ci) => 90,
            EvidenceKind::Contract | EvidenceKind::Anchor(AnchorKind::Contract) => 85,
            EvidenceKind::Entrypoint | EvidenceKind::Anchor(AnchorKind::Entrypoint) => 75,
            EvidenceKind::Anchor(AnchorKind::Experiment) => 65,
            EvidenceKind::Anchor(AnchorKind::Artifact) => 60,
            EvidenceKind::Anchor(AnchorKind::Infra) => 55,
            EvidenceKind::Boundary(_) => 40,
        };
        *dir_scores.entry(dir).or_insert(0) += weight;
    }
    map_rows.sort_by(|a, b| {
        let score_a = *dir_scores.get(&a.0).unwrap_or(&0);
        let score_b = *dir_scores.get(&b.0).unwrap_or(&0);
        score_b
            .cmp(&score_a)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    map_rows.truncate(map_limit);

    let root_fp = context_indexer::root_fingerprint(root_display);

    let mut emitted_boundaries: Vec<&BoundaryCandidate> = Vec::new();
    for boundary in &boundaries {
        if emitted_boundaries.len() >= DEFAULT_MAX_BOUNDARIES {
            break;
        }
        if !ev_file_index.contains_key(&boundary.file) {
            continue;
        }
        emitted_boundaries.push(boundary);
    }

    let mut emitted_entrypoints: Vec<&String> = Vec::new();
    if include_entrypoints {
        for file in &entrypoints {
            if emitted_entrypoints.len() >= DEFAULT_MAX_ENTRYPOINTS {
                break;
            }
            if !ev_file_index.contains_key(file) {
                continue;
            }
            emitted_entrypoints.push(file);
        }
    }

    let mut emitted_contracts: Vec<&String> = Vec::new();
    for file in &contracts {
        if emitted_contracts.len() >= DEFAULT_MAX_CONTRACTS {
            break;
        }
        if !ev_file_index.contains_key(file) {
            continue;
        }
        emitted_contracts.push(file);
    }

    #[derive(Clone)]
    struct EmittedFlow {
        contract_file: String,
        channel: String,
        direction: String,
        protocol: Option<String>,
        actor: Option<String>,
        confidence: f32,
        ev_id: String,
    }

    let mut emitted_flows: Vec<EmittedFlow> = Vec::new();
    for flow in &flows {
        if emitted_flows.len() >= DEFAULT_MAX_FLOWS {
            break;
        }

        let actor_from_mentions = channel_mentions
            .get(&flow.channel)
            .and_then(|hit| infer_actor_by_path(hit, &entrypoints));
        let (actor, actor_conf) = if let Some(actor) = actor_from_mentions {
            (Some(actor), 0.95)
        } else if let Some(actor) = infer_flow_actor(&flow.contract_file, &entrypoints) {
            (Some(actor), 0.85)
        } else {
            (None, 1.0)
        };
        let conf = if actor.is_some() { actor_conf } else { 1.0 };

        let Some(ev_id) = ev_file_index
            .get(&flow.contract_file)
            .or_else(|| actor.as_deref().and_then(|file| ev_file_index.get(file)))
            .cloned()
        else {
            continue;
        };

        emitted_flows.push(EmittedFlow {
            contract_file: flow.contract_file.clone(),
            channel: flow.channel.clone(),
            direction: flow.direction.as_str().to_string(),
            protocol: flow.protocol.clone(),
            actor,
            confidence: conf,
            ev_id,
        });
    }

    #[derive(Clone)]
    struct EmittedBroker {
        file: String,
        proto: String,
        confidence: f32,
        ev_id: String,
    }

    let mut emitted_brokers: Vec<EmittedBroker> = Vec::new();
    for broker in &brokers {
        if emitted_brokers.len() >= DEFAULT_MAX_BROKERS {
            break;
        }
        let Some(ev_id) = ev_file_index.get(&broker.file).cloned() else {
            continue;
        };
        emitted_brokers.push(EmittedBroker {
            file: broker.file.clone(),
            proto: broker.proto.clone(),
            confidence: broker.confidence,
            ev_id,
        });
    }

    let mut emitted_anchors: Vec<EmittedAnchor> = Vec::new();
    for anchor in &anchors {
        if emitted_anchors.len() >= DEFAULT_MAX_ANCHORS {
            break;
        }
        let Some(ev_id) = ev_file_index.get(&anchor.file).cloned() else {
            continue;
        };
        emitted_anchors.push(EmittedAnchor {
            kind: anchor.kind,
            label: anchor.label.clone(),
            file: anchor.file.clone(),
            confidence: anchor.confidence,
            ev_id,
        });
    }

    let pipeline_steps = build_pipeline_steps(root, &emitted_anchors).await;
    let areas = build_areas(
        &map_rows,
        map_depth,
        artifact_store_file.as_deref(),
        &emitted_anchors,
    );

    let mut used_ev_ids: HashSet<String> = HashSet::new();
    for anchor in &emitted_anchors {
        used_ev_ids.insert(anchor.ev_id.clone());
    }
    for step in &pipeline_steps {
        used_ev_ids.insert(step.ev_id.clone());
    }
    for area in &areas {
        if let Some(ev_id) = &area.ev_id {
            used_ev_ids.insert(ev_id.clone());
        }
    }
    for boundary in &emitted_boundaries {
        if let Some(ev_id) = ev_file_index.get(&boundary.file) {
            used_ev_ids.insert(ev_id.clone());
        }
    }
    for file in &emitted_entrypoints {
        if let Some(ev_id) = ev_file_index.get((*file).as_str()) {
            used_ev_ids.insert(ev_id.clone());
        }
    }
    for file in &emitted_contracts {
        if let Some(ev_id) = ev_file_index.get((*file).as_str()) {
            used_ev_ids.insert(ev_id.clone());
        }
    }
    for flow in &emitted_flows {
        used_ev_ids.insert(flow.ev_id.clone());
    }
    for broker in &emitted_brokers {
        used_ev_ids.insert(broker.ev_id.clone());
    }

    let mut dict_paths: BTreeSet<String> = BTreeSet::new();
    for (path, _) in &map_rows {
        dict_paths.insert(path.clone());
    }
    for anchor in &emitted_anchors {
        dict_paths.insert(anchor.label.clone());
        dict_paths.insert(anchor.file.clone());
    }
    for step in &pipeline_steps {
        dict_paths.insert(step.label.clone());
    }
    for area in &areas {
        dict_paths.insert(area.label.clone());
        dict_paths.insert(area.path.clone());
    }
    for boundary in &emitted_boundaries {
        dict_paths.insert(boundary.file.clone());
    }
    for file in &emitted_entrypoints {
        dict_paths.insert((**file).clone());
    }
    for file in &emitted_contracts {
        dict_paths.insert((**file).clone());
    }
    for flow in &emitted_flows {
        dict_paths.insert(flow.contract_file.clone());
        dict_paths.insert(flow.channel.clone());
        if let Some(actor) = &flow.actor {
            dict_paths.insert(actor.clone());
        }
    }
    for broker in &emitted_brokers {
        dict_paths.insert(broker.file.clone());
    }
    for (idx, ev) in evidence.iter().enumerate() {
        let ev_id = format!("ev{idx}");
        if !used_ev_ids.contains(&ev_id) {
            continue;
        }
        dict_paths.insert(ev.file.clone());
    }

    let mut cp = CognitivePack::new();
    cp.push_line("CPV1");
    cp.push_line(&format!("ROOT_FP {root_fp}"));
    cp.push_line(&format!("QUERY {}", json_string(&request.query)));

    for path in dict_paths {
        cp.dict_intern(path);
    }

    if !emitted_anchors.is_empty() {
        cp.push_line("S ANCHORS");
        for anchor in &emitted_anchors {
            let label_d = cp.dict_id(&anchor.label);
            let file_d = cp.dict_id(&anchor.file);
            let conf = format!("{:.2}", anchor.confidence.clamp(0.0, 1.0));
            cp.push_line(&format!(
                "ANCHOR kind={} label={label_d} file={file_d} conf={conf} ev={}",
                anchor.kind.as_str(),
                anchor.ev_id
            ));
        }
    }

    if !pipeline_steps.is_empty() {
        cp.push_line("S CANON");
        for step in &pipeline_steps {
            let label_d = cp.dict_id(&step.label);
            let conf = format!("{:.2}", step.confidence.clamp(0.0, 1.0));
            cp.push_line(&format!(
                "STEP kind={} label={label_d} conf={conf} ev={}",
                step.kind.as_str(),
                step.ev_id
            ));
        }
    }

    if !emitted_boundaries.is_empty() {
        cp.push_line("S BOUNDARIES");
        for boundary in &emitted_boundaries {
            let d = cp.dict_id(&boundary.file);
            let conf = format!("{:.2}", boundary.confidence.clamp(0.0, 1.0));
            let ev = ev_file_index
                .get(&boundary.file)
                .map(|id| format!(" ev={id}"))
                .unwrap_or_default();
            if ev.is_empty() {
                continue;
            }
            cp.push_line(&format!(
                "BOUNDARY kind={} file={d} conf={conf}{ev}",
                boundary.kind.as_str()
            ));
        }
    }

    cp.push_line("S MAP");
    for area in areas
        .iter()
        .filter(|area| !matches!(area.kind, "outputs" | "interfaces"))
    {
        let label_d = cp.dict_id(&area.label);
        let path_d = cp.dict_id(&area.path);
        let conf = format!("{:.2}", area.confidence.clamp(0.0, 1.0));
        let ev = area
            .ev_id
            .as_deref()
            .map(|id| format!(" ev={id}"))
            .unwrap_or_default();
        cp.push_line(&format!(
            "AREA kind={} label={label_d} path={path_d} conf={conf}{ev}",
            area.kind
        ));
    }
    for (path, files) in &map_rows {
        let d = cp.dict_id(path);
        cp.push_line(&format!("MAP path={d} files={files}"));
    }

    // Keep OUTPUTS after the general map: under tight budgets, we prefer preserving the
    // repo-wide sense map (docs/tooling/ci/core) longer than artifact-heavy areas.
    let output_areas: Vec<&EmittedArea> = areas
        .iter()
        .filter(|area| matches!(area.kind, "outputs" | "interfaces"))
        .collect();
    if !output_areas.is_empty() {
        cp.push_line("S OUTPUTS");
        for area in output_areas {
            let label_d = cp.dict_id(&area.label);
            let path_d = cp.dict_id(&area.path);
            let conf = format!("{:.2}", area.confidence.clamp(0.0, 1.0));
            let ev = area
                .ev_id
                .as_deref()
                .map(|id| format!(" ev={id}"))
                .unwrap_or_default();
            cp.push_line(&format!(
                "AREA kind={} label={label_d} path={path_d} conf={conf}{ev}",
                area.kind
            ));
        }
    }

    if !emitted_entrypoints.is_empty() {
        cp.push_line("S ENTRYPOINTS");
        for file in &emitted_entrypoints {
            let d = cp.dict_id(file.as_str());
            let ev = ev_file_index
                .get(file.as_str())
                .map(|id| format!(" ev={id}"))
                .unwrap_or_default();
            if ev.is_empty() {
                continue;
            }
            cp.push_line(&format!("ENTRY file={d}{ev}"));
        }
    }

    if !emitted_contracts.is_empty() {
        cp.push_line("S CONTRACTS");
        for file in &emitted_contracts {
            let d = cp.dict_id(file.as_str());
            let ev = ev_file_index
                .get(file.as_str())
                .map(|id| format!(" ev={id}"))
                .unwrap_or_default();
            if ev.is_empty() {
                continue;
            }
            cp.push_line(&format!(
                "CONTRACT kind={} file={d}{ev}",
                contract_kind(file)
            ));
        }
    }

    if !emitted_flows.is_empty() {
        cp.push_line("S FLOWS");
        for flow in &emitted_flows {
            let contract_d = cp.dict_id(&flow.contract_file);
            let chan_d = cp.dict_id(&flow.channel);
            let actor_field = flow
                .actor
                .as_deref()
                .map(|file| format!(" actor={}", cp.dict_id(file)))
                .unwrap_or_default();
            let proto_field = flow
                .protocol
                .as_deref()
                .map(|p| format!(" proto={p}"))
                .unwrap_or_default();
            cp.push_line(&format!(
                "FLOW contract={contract_d} chan={chan_d} dir={}{}{} conf={:.2} ev={}",
                flow.direction,
                proto_field,
                actor_field,
                flow.confidence.clamp(0.0, 1.0),
                flow.ev_id
            ));
        }
    }

    if !emitted_brokers.is_empty() {
        cp.push_line("S BROKERS");
        for broker in &emitted_brokers {
            let d = cp.dict_id(&broker.file);
            let conf = format!("{:.2}", broker.confidence.clamp(0.0, 1.0));
            cp.push_line(&format!(
                "BROKER proto={} file={d} conf={conf} ev={}",
                broker.proto, broker.ev_id
            ));
        }
    }

    if !evidence.is_empty() && !used_ev_ids.is_empty() {
        cp.push_line("S EVIDENCE");
        for (idx, ev) in evidence.iter().enumerate() {
            let ev_id = format!("ev{idx}");
            if !used_ev_ids.contains(&ev_id) {
                continue;
            }
            let d = cp.dict_id(&ev.file);
            let kind = match ev.kind {
                EvidenceKind::Entrypoint => "entrypoint".to_string(),
                EvidenceKind::Contract => "contract".to_string(),
                EvidenceKind::Boundary(kind) => format!("boundary.{}", kind.as_str()),
                EvidenceKind::Anchor(kind) => format!("anchor.{}", kind.as_str()),
            };
            let hash = ev
                .source_hash
                .as_deref()
                .map(|h| format!(" sha256={h}"))
                .unwrap_or_default();
            cp.push_line(&format!(
                "EV {ev_id} kind={kind} file={d} L{}-L{}{}",
                ev.start_line, ev.end_line, hash,
            ));
        }
    }

    let nba = evidence
        .iter()
        .enumerate()
        .find_map(|(idx, ev)| {
            let ev_id = format!("ev{idx}");
            used_ev_ids.contains(&ev_id).then(|| {
                let d = cp.dict_id(&ev.file);
                format!(
                    "NBA evidence_fetch ev={ev_id} file={d} L{}-L{}",
                    ev.start_line, ev.end_line,
                )
            })
        })
        .unwrap_or_else(|| "NBA map".to_string());
    cp.push_line(&nba);

    let mut result = MeaningPackResult {
        version: VERSION,
        query: request.query.clone(),
        format: "cpv1".to_string(),
        pack: cp.render(),
        budget: MeaningPackBudget {
            max_chars,
            used_chars: 0,
            truncated: false,
            truncation: None,
        },
    };

    trim_to_budget(&mut result)?;
    Ok(result)
}

fn trim_to_budget(result: &mut MeaningPackResult) -> anyhow::Result<()> {
    let max_chars = result.budget.max_chars;
    let used = enforce_max_chars(
        result,
        max_chars,
        |inner, used| inner.budget.used_chars = used,
        |inner| {
            inner.budget.truncated = true;
            inner.budget.truncation = Some(BudgetTruncation::MaxChars);
        },
        |inner| shrink_pack(&mut inner.pack),
    )?;
    result.budget.used_chars = used;
    Ok(())
}

async fn collect_evidence(
    root: &Path,
    anchors: &[AnchorCandidate],
    entrypoints: &[String],
    contracts: &[String],
    boundaries: &[BoundaryCandidate],
    flows: &[FlowEdge],
    brokers: &[BrokerCandidate],
) -> Vec<EvidenceItem> {
    let mut candidates: Vec<(EvidenceKind, String)> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();

    for anchor in anchors.iter().take(DEFAULT_MAX_EVIDENCE) {
        if !seen.insert(anchor.file.as_str()) {
            continue;
        }
        candidates.push((EvidenceKind::Anchor(anchor.kind), anchor.file.clone()));
    }

    // Ensure event-driven claims have at least one evidence anchor (contract and/or actor).
    let mut must_contracts: Vec<String> = Vec::new();
    let mut must_entrypoints: Vec<String> = Vec::new();
    for flow in flows {
        if must_contracts.len() < 2 && !must_contracts.iter().any(|c| c == &flow.contract_file) {
            must_contracts.push(flow.contract_file.clone());
        }
        if must_entrypoints.len() < 2 {
            if let Some(actor) = infer_flow_actor(&flow.contract_file, entrypoints) {
                if !must_entrypoints.iter().any(|e| e == &actor) {
                    must_entrypoints.push(actor);
                }
            }
        }
        if must_contracts.len() >= 2 && must_entrypoints.len() >= 2 {
            break;
        }
    }
    for file in &must_contracts {
        if !seen.insert(file.as_str()) {
            continue;
        }
        candidates.push((EvidenceKind::Contract, file.clone()));
    }
    for file in &must_entrypoints {
        if !seen.insert(file.as_str()) {
            continue;
        }
        candidates.push((EvidenceKind::Entrypoint, file.clone()));
    }

    // Ensure broker config claims have evidence anchors.
    for broker in brokers.iter().take(2) {
        if !seen.insert(broker.file.as_str()) {
            continue;
        }
        candidates.push((
            EvidenceKind::Boundary(BoundaryKind::Config),
            broker.file.clone(),
        ));
    }

    for file in entrypoints.iter().take(DEFAULT_MAX_EVIDENCE) {
        if !seen.insert(file.as_str()) {
            continue;
        }
        candidates.push((EvidenceKind::Entrypoint, file.clone()));
    }
    for file in contracts
        .iter()
        .take(DEFAULT_MAX_EVIDENCE.saturating_sub(candidates.len()))
    {
        if !seen.insert(file.as_str()) {
            continue;
        }
        candidates.push((EvidenceKind::Contract, file.clone()));
    }
    for boundary in boundaries
        .iter()
        .take(DEFAULT_MAX_EVIDENCE.saturating_sub(candidates.len()))
    {
        if !seen.insert(boundary.file.as_str()) {
            continue;
        }
        candidates.push((EvidenceKind::Boundary(boundary.kind), boundary.file.clone()));
    }

    let mut out = Vec::new();
    for (kind, rel) in candidates.into_iter().take(DEFAULT_MAX_EVIDENCE) {
        let abs = root.join(&rel);
        let (hash, lines) = hash_and_count_lines(&abs).await.ok().unwrap_or_default();
        let (start_line, end_line) = match kind {
            EvidenceKind::Anchor(anchor_kind) => {
                let (start, end) =
                    anchor_evidence_window(root, &rel, anchor_kind, DEFAULT_EVIDENCE_END_LINE)
                        .await;
                let file_lines = lines.max(1);
                let start = start.clamp(1, file_lines);
                let end = end.clamp(start, file_lines);
                (start, end)
            }
            _ => (1, DEFAULT_EVIDENCE_END_LINE.min(lines.max(1))),
        };
        out.push(EvidenceItem {
            kind,
            file: rel,
            start_line,
            end_line,
            source_hash: if hash.is_empty() { None } else { Some(hash) },
        });
    }
    out
}

fn select_repo_anchors(
    files: &[String],
    entrypoints: &[String],
    contracts: &[String],
    boundaries: &[BoundaryCandidate],
    artifact_store_file: Option<&str>,
    hints: &QueryHints,
    max_anchors: usize,
) -> Vec<AnchorCandidate> {
    let mut out: Vec<AnchorCandidate> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    if let Some(file) = best_canon_doc(files) {
        if seen.insert(file.clone()) {
            out.push(AnchorCandidate {
                kind: AnchorKind::Canon,
                label: "Canon: start here".to_string(),
                file,
                confidence: 0.9,
            });
        }
    }

    if let Some(file) = best_howto_file(files) {
        if seen.insert(file.clone()) {
            out.push(AnchorCandidate {
                kind: AnchorKind::HowTo,
                label: "How-to: run / test".to_string(),
                file,
                confidence: 0.85,
            });
        }
    }

    if let Some(file) = best_ci_file(files) {
        if seen.insert(file.clone()) {
            out.push(AnchorCandidate {
                kind: AnchorKind::Ci,
                label: "CI: gates".to_string(),
                file,
                confidence: 0.78,
            });
        }
    }

    if let Some(file) = best_contract_file(contracts) {
        if seen.insert(file.clone()) {
            out.push(AnchorCandidate {
                kind: AnchorKind::Contract,
                label: "Contract: interfaces".to_string(),
                file,
                confidence: 0.82,
            });
        }
    }

    if let Some(file) = entrypoints.first().cloned() {
        if seen.insert(file.clone()) {
            out.push(AnchorCandidate {
                kind: AnchorKind::Entrypoint,
                label: "Entrypoint: code".to_string(),
                file,
                confidence: 0.78,
            });
        }
    }

    if let Some(file) = best_experiment_file(files) {
        if seen.insert(file.clone()) {
            out.push(AnchorCandidate {
                kind: AnchorKind::Experiment,
                label: "Experiments: baselines".to_string(),
                file,
                confidence: 0.76,
            });
        }
    }

    if let Some(file) = best_infra_file(boundaries) {
        if seen.insert(file.clone()) {
            out.push(AnchorCandidate {
                kind: AnchorKind::Infra,
                label: "Infra: deploy".to_string(),
                file,
                confidence: 0.8,
            });
        }
    }

    if let Some(file) = artifact_store_file.map(|v| v.to_string()) {
        if seen.insert(file.clone()) {
            out.push(AnchorCandidate {
                kind: AnchorKind::Artifact,
                label: "Artifacts: outputs".to_string(),
                file,
                confidence: 0.72,
            });
        }
    }

    fn anchor_priority(kind: AnchorKind, hints: &QueryHints) -> i32 {
        let base = match kind {
            AnchorKind::Canon => 100,
            AnchorKind::HowTo => 95,
            AnchorKind::Contract => 80,
            AnchorKind::Ci => 78,
            AnchorKind::Experiment => 70,
            AnchorKind::Artifact => 68,
            AnchorKind::Infra => 60,
            AnchorKind::Entrypoint => 55,
        };
        let boost = match kind {
            AnchorKind::Entrypoint => i32::from(hints.wants_entrypoints) * 25,
            AnchorKind::Contract => i32::from(hints.wants_contracts) * 20,
            AnchorKind::Experiment => i32::from(hints.wants_experiments) * 20,
            AnchorKind::Artifact => i32::from(hints.wants_artifacts) * 20,
            AnchorKind::Infra => i32::from(hints.wants_infra) * 15,
            AnchorKind::Ci => i32::from(hints.wants_ci) * 15,
            _ => 0,
        };
        base + boost
    }

    // Deterministic ordering: stable tie-breakers (kind + file).
    out.sort_by(|a, b| {
        anchor_priority(b.kind, hints)
            .cmp(&anchor_priority(a.kind, hints))
            .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
            .then_with(|| a.file.cmp(&b.file))
    });
    out.truncate(max_anchors.max(1));
    out
}

pub(crate) fn best_artifact_store_evidence_file(files: &[String]) -> Option<String> {
    let mut candidates: Vec<(usize, usize, &String)> = Vec::new();
    for file in files {
        let lc = file.to_ascii_lowercase();
        if !is_artifact_scope(&lc) {
            continue;
        }

        let scope = lc.split('/').next().unwrap_or("");
        let basename = lc.rsplit('/').next().unwrap_or(lc.as_str());
        let depth = lc.matches('/').count();

        let is_doc = basename.ends_with(".md")
            || basename.ends_with(".txt")
            || basename.ends_with(".json")
            || basename.ends_with(".yaml")
            || basename.ends_with(".yml")
            || basename.ends_with(".toml");
        if !is_doc {
            continue;
        }

        let file_rank = match basename {
            "readme.md" => 0usize,
            "readme.txt" => 1,
            "index.md" | "overview.md" => 2,
            "manifest.json" | "manifest.yaml" | "manifest.yml" => 3,
            "metadata.json" => 4,
            _ => 10,
        };

        let scope_rank = artifact_scope_rank(scope);
        let rank = scope_rank.saturating_mul(100).saturating_add(file_rank);
        candidates.push((rank, depth, file));
    }
    candidates.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(b.2))
    });
    candidates.first().map(|c| (*c.2).clone())
}

pub(crate) fn best_canon_doc(files: &[String]) -> Option<String> {
    let mut candidates: Vec<(usize, &String)> = Vec::new();
    for file in files {
        let lc = file.to_ascii_lowercase();
        if is_artifact_scope(&lc) {
            continue;
        }
        let basename = lc.rsplit('/').next().unwrap_or(lc.as_str());
        let is_root = lc == basename;
        let is_doc = lc.ends_with(".md") || lc.ends_with(".rst") || lc.ends_with(".txt");
        if !is_doc {
            continue;
        }

        let rank = if is_root && matches!(basename, "readme.md" | "readme.rst" | "readme.txt") {
            Some(0usize)
        } else if matches!(basename, "readme.md" | "readme.rst" | "readme.txt") {
            Some(1usize)
        } else if is_root && matches!(basename, "philosophy.md" | "goals.md" | "agents.md") {
            Some(2usize)
        } else if lc.starts_with("docs/")
            && matches!(
                basename,
                "quick_start.md"
                    | "quickstart.md"
                    | "getting_started.md"
                    | "installation.md"
                    | "install.md"
                    | "usage.md"
                    | "overview.md"
                    | "architecture.md"
            )
        {
            Some(3usize)
        } else if lc.contains("philosophy") || lc.contains("goals") || lc.contains("architecture") {
            Some(4usize)
        } else if lc.starts_with("docs/") {
            Some(5usize)
        } else {
            None
        };

        if let Some(rank) = rank {
            candidates.push((rank, file));
        }
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    candidates.first().map(|(_, f)| (*f).clone())
}

pub(crate) fn best_howto_file(files: &[String]) -> Option<String> {
    let mut candidates: Vec<(usize, &String)> = Vec::new();
    for file in files {
        let lc = file.to_ascii_lowercase();
        if is_artifact_scope(&lc) {
            continue;
        }
        let basename = lc.rsplit('/').next().unwrap_or(lc.as_str());
        let is_root = lc == basename;

        let rank = if is_root
            && matches!(
                basename,
                "makefile"
                    | "justfile"
                    | "taskfile.yml"
                    | "taskfile.yaml"
                    | "docker-compose.yml"
                    | "docker-compose.yaml"
                    | "compose.yml"
                    | "compose.yaml"
            ) {
            Some(0usize)
        } else if lc.starts_with("scripts/")
            && matches!(
                basename,
                "validate_contracts.sh"
                    | "validate.sh"
                    | "check.sh"
                    | "test.sh"
                    | "ci.sh"
                    | "verify.sh"
            )
        {
            Some(1usize)
        } else if is_root
            && matches!(
                basename,
                "package.json" | "pyproject.toml" | "go.mod" | "cargo.toml"
            )
        {
            Some(2usize)
        } else if (lc.starts_with(".github/workflows/")
            && (lc.ends_with(".yml") || lc.ends_with(".yaml")))
            || (is_root && basename == ".gitlab-ci.yml")
        {
            Some(3usize)
        } else {
            None
        };

        if let Some(rank) = rank {
            candidates.push((rank, file));
        }
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    candidates.first().map(|(_, f)| (*f).clone())
}

fn best_ci_file(files: &[String]) -> Option<String> {
    let mut candidates: Vec<(usize, &String)> = Vec::new();
    for file in files {
        let lc = file.to_ascii_lowercase();
        if is_artifact_scope(&lc) {
            continue;
        }
        if !is_ci_config_candidate(&lc) {
            continue;
        }

        let basename = lc.rsplit('/').next().unwrap_or(lc.as_str());
        let is_root = lc == basename;
        let rank = if (is_root && basename == ".gitlab-ci.yml")
            || (lc.starts_with(".github/workflows/")
                && (basename.contains("ci")
                    || basename.contains("test")
                    || basename.contains("build")
                    || basename.contains("preflight")))
        {
            0usize
        } else if lc.starts_with(".github/workflows/") {
            1usize
        } else if is_root {
            2usize
        } else {
            3usize
        };

        candidates.push((rank, file));
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    candidates.first().map(|(_, f)| (*f).clone())
}

pub(crate) fn best_experiment_file(files: &[String]) -> Option<String> {
    let mut candidates: Vec<(usize, &String)> = Vec::new();
    for file in files {
        let lc = file.to_ascii_lowercase();
        if is_artifact_scope(&lc) {
            continue;
        }

        let basename = lc.rsplit('/').next().unwrap_or(lc.as_str());

        let in_experiment_scope = lc.starts_with("experiments/")
            || lc.starts_with("experiment/")
            || lc.starts_with("baselines/")
            || lc.starts_with("baseline/")
            || lc.starts_with("bench/")
            || lc.starts_with("benches/")
            || lc.starts_with("notebooks/")
            || lc.contains("/experiments/")
            || lc.contains("/experiment/")
            || lc.contains("/baselines/")
            || lc.contains("/baseline/")
            || lc.contains("/bench/")
            || lc.contains("/benches/")
            || lc.contains("/eval/")
            || lc.contains("/evaluation/")
            || lc.contains("/ablations/")
            || lc.contains("/analysis/")
            || lc.contains("/notebooks/");
        if !in_experiment_scope {
            continue;
        }

        let is_doc = lc.ends_with(".md") || lc.ends_with(".rst") || lc.ends_with(".txt");

        let rank = if matches!(basename, "readme.md" | "readme.rst" | "readme.txt") {
            Some(0usize)
        } else if is_doc {
            Some(1usize)
        } else if lc.ends_with(".yaml") || lc.ends_with(".yml") || lc.ends_with(".json") {
            Some(2usize)
        } else {
            None
        };
        if let Some(rank) = rank {
            candidates.push((rank, file));
        }
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    candidates.first().map(|(_, f)| (*f).clone())
}

pub(crate) fn best_contract_file(contracts: &[String]) -> Option<String> {
    let mut candidates: Vec<(usize, &String)> = Vec::new();
    for file in contracts {
        let kind = contract_kind(file);
        let rank = match kind {
            "asyncapi" => 0usize,
            "openapi" => 1usize,
            "proto" => 2usize,
            "jsonschema" => 3usize,
            _ => 4usize,
        };
        candidates.push((rank, file));
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    candidates.first().map(|(_, f)| (*f).clone())
}

pub(crate) fn best_infra_file(boundaries: &[BoundaryCandidate]) -> Option<String> {
    boundaries.iter().find_map(|b| {
        let lc = b.file.to_ascii_lowercase();
        let is_infra = lc.contains("/k8s/")
            || lc.starts_with("k8s/")
            || lc.contains("/kubernetes/")
            || lc.starts_with("kubernetes/")
            || lc.contains("/manifests/")
            || lc.starts_with("manifests/")
            || lc.contains("/deploy/")
            || lc.starts_with("deploy/")
            || lc.contains("/gitops/")
            || lc.starts_with("gitops/")
            || lc.contains("/argocd/")
            || lc.starts_with("argocd/")
            || lc.contains("/argo/")
            || lc.starts_with("argo/")
            || lc.contains("/flux/")
            || lc.starts_with("flux/")
            || lc.contains("/clusters/")
            || lc.starts_with("clusters/")
            || lc.contains("/infra/")
            || lc.starts_with("infra/")
            || lc.contains("/terraform/")
            || lc.starts_with("terraform/")
            || lc.contains("/charts/")
            || lc.starts_with("charts/")
            || lc.contains("/helm/")
            || lc.ends_with(".tf")
            || lc.ends_with(".tfvars")
            || lc.ends_with(".hcl")
            || lc.ends_with("kustomization.yaml")
            || lc.ends_with("kustomization.yml")
            || lc.ends_with("helmfile.yaml")
            || lc.ends_with("helmfile.yml")
            || lc.ends_with("helmrelease.yaml")
            || lc.ends_with("helmrelease.yml")
            || lc.ends_with("application.yaml")
            || lc.ends_with("application.yml")
            || lc.ends_with("applicationset.yaml")
            || lc.ends_with("applicationset.yml")
            || lc.ends_with("skaffold.yaml")
            || lc.ends_with("skaffold.yml")
            || lc.ends_with("werf.yaml")
            || lc.ends_with("werf.yml")
            || lc.ends_with("devspace.yaml")
            || lc.ends_with("devspace.yml")
            || lc == "tiltfile";
        is_infra.then(|| b.file.clone())
    })
}

async fn augment_k8s_manifest_boundaries(
    root: &Path,
    files: &[String],
    boundaries: &mut Vec<BoundaryCandidate>,
) {
    const MAX_CANDIDATES: usize = 12;
    const MAX_ADDED: usize = 3;
    const MAX_READ_BYTES: usize = 48 * 1024;

    let mut seen: HashSet<&str> = boundaries.iter().map(|b| b.file.as_str()).collect();
    let mut candidates: Vec<(usize, &String)> = Vec::new();

    for file in files {
        let lc = file.to_ascii_lowercase();
        if !(lc.ends_with(".yaml") || lc.ends_with(".yml")) {
            continue;
        }
        if lc.starts_with(".github/workflows/") {
            continue;
        }
        if lc.starts_with("docs/") || lc.contains("/docs/") {
            continue;
        }
        if seen.contains(file.as_str()) {
            continue;
        }

        let rank = if lc.contains("kafka") || lc.contains("nats") || lc.contains("rabbit") {
            0usize
        } else if lc.contains("deployment")
            || lc.contains("statefulset")
            || lc.contains("service")
            || lc.contains("ingress")
        {
            1usize
        } else {
            2usize
        };
        candidates.push((rank, file));
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    candidates.truncate(MAX_CANDIDATES);

    let mut to_add: Vec<BoundaryCandidate> = Vec::new();
    for (_, file) in candidates {
        if to_add.len() >= MAX_ADDED {
            break;
        }
        let Some(content) = read_file_prefix_utf8(root, file, MAX_READ_BYTES).await else {
            continue;
        };
        let content_lc = content.to_ascii_lowercase();
        let looks_like_k8s = content_lc.contains("\napiversion:")
            || content_lc.starts_with("apiversion:")
            || content_lc.contains("\nkind:")
            || content_lc.starts_with("kind:");
        if !looks_like_k8s {
            continue;
        }
        if !seen.insert(file.as_str()) {
            continue;
        }
        to_add.push(BoundaryCandidate {
            kind: BoundaryKind::Config,
            file: file.clone(),
            confidence: 0.72,
        });
    }

    if to_add.is_empty() {
        return;
    }

    let insert_at = boundaries
        .iter()
        .position(|b| matches!(b.kind, BoundaryKind::Db | BoundaryKind::Event))
        .unwrap_or(boundaries.len());
    for (offset, b) in to_add.into_iter().enumerate() {
        boundaries.insert(insert_at + offset, b);
    }
}

pub(crate) async fn anchor_evidence_window(
    root: &Path,
    rel: &str,
    kind: AnchorKind,
    max_window_lines: usize,
) -> (usize, usize) {
    const MAX_READ_BYTES: usize = 64 * 1024;
    let Some(content) = read_file_prefix_utf8(root, rel, MAX_READ_BYTES).await else {
        return (1, max_window_lines.max(1));
    };
    let lines = content.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return (1, max_window_lines.max(1));
    }

    let lc_lines = lines
        .iter()
        .map(|line| line.to_ascii_lowercase())
        .collect::<Vec<_>>();

    let best_idx = match kind {
        AnchorKind::Canon => find_first_heading_like(
            &lc_lines,
            &[
                "quick start",
                "getting started",
                "usage",
                "install",
                "overview",
                "start here",
                "architecture",
                "goals",
                "philosophy",
            ],
        ),
        AnchorKind::HowTo => find_first_heading_like(
            &lc_lines,
            &[
                "how to run",
                "howto",
                "usage",
                "run",
                "test",
                "build",
                "lint",
                "fmt",
                "format",
                "serve",
            ],
        )
        .or_else(|| find_first_commandish(&lc_lines)),
        AnchorKind::Ci => find_first_heading_like(
            &lc_lines,
            &["jobs", "job", "steps", "workflow", "pipeline", "ci"],
        )
        .or_else(|| find_first_commandish(&lc_lines)),
        AnchorKind::Artifact => find_first_heading_like(
            &lc_lines,
            &[
                "artifacts",
                "artifact",
                "results",
                "runs",
                "outputs",
                "checkpoints",
                "layout",
                "naming",
            ],
        ),
        AnchorKind::Experiment => find_first_heading_like(
            &lc_lines,
            &[
                "experiments",
                "experiment",
                "baselines",
                "baseline",
                "bench",
                "benches",
                "benchmark",
                "evaluation",
                "eval",
                "ablation",
                "ablations",
                "analysis",
            ],
        )
        .or_else(|| find_first_commandish(&lc_lines)),
        _ => None,
    };

    let idx = best_idx.unwrap_or(0);
    let start = idx.saturating_sub(3) + 1;
    let end = start.saturating_add(max_window_lines.saturating_sub(1));
    (start.max(1), end.max(start.max(1)))
}

fn find_first_heading_like(lines_lc: &[String], needles: &[&str]) -> Option<usize> {
    for (idx, line) in lines_lc.iter().enumerate() {
        let trimmed = line.trim_start();
        let is_heading = trimmed.starts_with('#')
            || trimmed.starts_with("##")
            || trimmed.starts_with("###")
            || trimmed.starts_with("==")
            || trimmed.starts_with("--");
        if !is_heading {
            continue;
        }
        if needles.iter().any(|n| trimmed.contains(n)) {
            return Some(idx);
        }
    }

    for (idx, line) in lines_lc.iter().enumerate() {
        let trimmed = line.trim_start();
        if needles.iter().any(|n| trimmed.contains(n)) {
            return Some(idx);
        }
    }

    None
}

fn find_first_commandish(lines_lc: &[String]) -> Option<usize> {
    const TOKENS: [&str; 10] = [
        "cargo test",
        "cargo build",
        "cargo run",
        "npm run",
        "pnpm",
        "yarn",
        "pip install",
        "python -m",
        "make ",
        "just ",
    ];

    for (idx, line) in lines_lc.iter().enumerate() {
        let trimmed = line.trim_start();
        if TOKENS.iter().any(|t| trimmed.contains(t)) {
            return Some(idx);
        }
        let looks_like_target = trimmed
            .split_once(':')
            .map(|(lhs, _)| {
                let lhs = lhs.trim();
                !lhs.is_empty()
                    && lhs.len() <= 32
                    && lhs
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            })
            .unwrap_or(false);
        if looks_like_target
            && (trimmed.starts_with("test:")
                || trimmed.starts_with("run:")
                || trimmed.starts_with("build:")
                || trimmed.starts_with("lint:")
                || trimmed.starts_with("fmt:"))
        {
            return Some(idx);
        }
        let looks_like_yaml_run = trimmed.starts_with("- run:") || trimmed.starts_with("run:");
        if looks_like_yaml_run {
            return Some(idx);
        }
    }

    None
}

async fn build_pipeline_steps(root: &Path, anchors: &[EmittedAnchor]) -> Vec<EmittedPipelineStep> {
    const MAX_READ_BYTES: usize = 96 * 1024;
    const MAX_STEPS: usize = 6;

    let mut selected: HashMap<PipelineStepKind, EmittedPipelineStep> = HashMap::new();

    let mut sources: Vec<&EmittedAnchor> = Vec::new();
    if let Some(howto) = anchors.iter().find(|a| matches!(a.kind, AnchorKind::HowTo)) {
        sources.push(howto);
    }
    if let Some(ci) = anchors.iter().find(|a| matches!(a.kind, AnchorKind::Ci)) {
        if sources
            .iter()
            .all(|existing| existing.file.as_str() != ci.file.as_str())
        {
            sources.push(ci);
        }
    }
    if let Some(canon) = anchors.iter().find(|a| matches!(a.kind, AnchorKind::Canon)) {
        if sources
            .first()
            .map(|a| a.file.as_str() != canon.file.as_str())
            .unwrap_or(true)
        {
            sources.push(canon);
        }
    }
    if let Some(experiment) = anchors
        .iter()
        .find(|a| matches!(a.kind, AnchorKind::Experiment))
    {
        if sources
            .iter()
            .all(|existing| existing.file.as_str() != experiment.file.as_str())
        {
            sources.push(experiment);
        }
    }

    for anchor in sources {
        let Some(content) = read_file_prefix_utf8(root, &anchor.file, MAX_READ_BYTES).await else {
            continue;
        };
        let mut candidates = extract_pipeline_candidates(&anchor.file, &content);
        candidates.sort_by(|a, b| {
            pipeline_step_rank(a.kind)
                .cmp(&pipeline_step_rank(b.kind))
                .then_with(|| a.label.cmp(&b.label))
        });
        for cand in candidates {
            selected
                .entry(cand.kind)
                .or_insert_with(|| EmittedPipelineStep {
                    kind: cand.kind,
                    label: cand.label,
                    confidence: cand.confidence,
                    ev_id: anchor.ev_id.clone(),
                });
            if selected.len() >= MAX_STEPS {
                break;
            }
        }
        if selected.len() >= MAX_STEPS {
            break;
        }
    }

    let mut out: Vec<EmittedPipelineStep> = selected.into_values().collect();
    out.sort_by(|a, b| {
        pipeline_step_rank(a.kind)
            .cmp(&pipeline_step_rank(b.kind))
            .then_with(|| a.label.cmp(&b.label))
    });
    out.truncate(MAX_STEPS);
    out
}

fn extract_pipeline_candidates(file: &str, content: &str) -> Vec<PipelineStepCandidate> {
    let file_lc = file.to_ascii_lowercase();
    let basename = file_lc.rsplit('/').next().unwrap_or(file_lc.as_str());

    if basename == "makefile" {
        return extract_make_like_pipeline("make", content);
    }
    if basename == "justfile" {
        return extract_make_like_pipeline("just", content);
    }
    if basename == "package.json" {
        return extract_package_json_pipeline(content);
    }

    extract_commandish_pipeline(content)
}

fn extract_make_like_pipeline(tool: &str, content: &str) -> Vec<PipelineStepCandidate> {
    let mut out: Vec<PipelineStepCandidate> = Vec::new();
    let mut seen: HashSet<(PipelineStepKind, String)> = HashSet::new();

    for raw in content.lines() {
        let line = raw.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((lhs, _)) = line.split_once(':') else {
            continue;
        };
        let target = lhs.trim();
        if target.is_empty() || target.len() > 32 || target.contains(char::is_whitespace) {
            continue;
        }
        if !target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            continue;
        }
        let target_lc = target.to_ascii_lowercase();
        let Some(kind) = pipeline_kind_for_target(&target_lc) else {
            continue;
        };
        let label = format!("{tool} {target}");
        if !seen.insert((kind, label.clone())) {
            continue;
        }
        out.push(PipelineStepCandidate {
            kind,
            label,
            confidence: 0.9,
        });
    }

    out.sort_by(|a, b| {
        pipeline_step_rank(a.kind)
            .cmp(&pipeline_step_rank(b.kind))
            .then_with(|| a.label.cmp(&b.label))
    });
    out
}

fn extract_package_json_pipeline(content: &str) -> Vec<PipelineStepCandidate> {
    let mut out: Vec<PipelineStepCandidate> = Vec::new();
    let mut seen: HashSet<(PipelineStepKind, String)> = HashSet::new();

    if let Ok(json) = serde_json::from_str::<serde_json::Value>(content) {
        if let Some(scripts) = json.get("scripts").and_then(|v| v.as_object()) {
            for (name, _) in scripts {
                let name_lc = name.trim().to_ascii_lowercase();
                let Some(kind) = pipeline_kind_for_target(&name_lc) else {
                    continue;
                };
                let label = format!("npm run {name}");
                if !seen.insert((kind, label.clone())) {
                    continue;
                }
                out.push(PipelineStepCandidate {
                    kind,
                    label,
                    confidence: 0.85,
                });
            }
        }
    }

    out.sort_by(|a, b| {
        pipeline_step_rank(a.kind)
            .cmp(&pipeline_step_rank(b.kind))
            .then_with(|| a.label.cmp(&b.label))
    });
    out
}

fn extract_commandish_pipeline(content: &str) -> Vec<PipelineStepCandidate> {
    let mut out: Vec<PipelineStepCandidate> = Vec::new();
    let mut seen: HashSet<PipelineStepKind> = HashSet::new();

    for raw in content.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let trimmed = trimmed.strip_prefix('$').unwrap_or(trimmed).trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let trimmed = trimmed
            .strip_prefix("- run:")
            .or_else(|| trimmed.strip_prefix("run:"))
            .map(str::trim)
            .unwrap_or(trimmed);

        let lc = trimmed.to_ascii_lowercase();
        let Some(kind) = pipeline_kind_for_command(&lc) else {
            continue;
        };
        if !seen.insert(kind) {
            continue;
        }
        out.push(PipelineStepCandidate {
            kind,
            label: truncate_pipeline_label(trimmed, 80),
            confidence: 0.75,
        });
        if seen.len() >= 6 {
            break;
        }
    }

    out.sort_by(|a, b| {
        pipeline_step_rank(a.kind)
            .cmp(&pipeline_step_rank(b.kind))
            .then_with(|| a.label.cmp(&b.label))
    });
    out
}

fn truncate_pipeline_label(value: &str, max_chars: usize) -> String {
    let value = value.trim();
    if max_chars < 8 {
        return value.chars().take(max_chars).collect();
    }
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect()
}

fn pipeline_kind_for_target(name_lc: &str) -> Option<PipelineStepKind> {
    let name = name_lc.trim();
    if name.is_empty() {
        return None;
    }
    if name == "doctor" || name == "preflight" {
        return Some(PipelineStepKind::Setup);
    }
    if name == "setup"
        || name == "install"
        || name == "bootstrap"
        || name == "deps"
        || name == "dep"
        || name == "init"
    {
        return Some(PipelineStepKind::Setup);
    }
    if name == "build" {
        return Some(PipelineStepKind::Build);
    }
    if name == "run" || name == "start" || name == "serve" || name == "dev" {
        return Some(PipelineStepKind::Run);
    }
    if name.starts_with("test") || name == "unit" || name == "integration" {
        return Some(PipelineStepKind::Test);
    }
    if name == "check"
        || name == "validate"
        || name == "verify"
        || name == "smoke"
        || name == "skeleton"
        || name == "ci"
        || name.starts_with("ci-")
        || name.contains("smoke")
        || name.contains("gate")
    {
        return Some(PipelineStepKind::Test);
    }
    if name.contains("eval") || name.contains("bench") || name.contains("benchmark") {
        return Some(PipelineStepKind::Eval);
    }
    if name == "lint" {
        return Some(PipelineStepKind::Lint);
    }
    if name == "fmt" || name == "format" {
        return Some(PipelineStepKind::Format);
    }
    None
}

fn pipeline_kind_for_command(line_lc: &str) -> Option<PipelineStepKind> {
    let line = line_lc.trim();
    if line.is_empty() {
        return None;
    }
    if line.contains("git lfs install") || line.contains("git lfs pull") {
        return Some(PipelineStepKind::Setup);
    }
    if line.contains("pip install")
        || line.contains("poetry install")
        || line.contains("uv pip")
        || line.contains("pnpm install")
        || line.contains("npm install")
        || line.contains("yarn install")
    {
        return Some(PipelineStepKind::Setup);
    }
    if line.contains("cargo build") || line.contains("npm run build") || line.contains("pnpm build")
    {
        return Some(PipelineStepKind::Build);
    }
    if line.contains("go build") || line.contains("dotnet build") {
        return Some(PipelineStepKind::Build);
    }
    if line.contains("cmake -s") || line.contains("cmake --build") || line.contains("ninja") {
        return Some(PipelineStepKind::Build);
    }
    if line.contains("cargo run")
        || line.contains("python -m")
        || line.starts_with("python ")
        || line.starts_with("python3 ")
        || line.contains("go run")
        || line.contains("npm run dev")
        || line.contains("npm start")
        || line.contains("pnpm dev")
        || line.contains("yarn dev")
    {
        return Some(PipelineStepKind::Run);
    }
    if line.contains("cargo test")
        || line.contains("pytest")
        || line.contains("ctest")
        || line.contains("go test")
        || line.contains("dotnet test")
        || line.contains("npm test")
        || line.contains("pnpm test")
        || line.contains("yarn test")
    {
        return Some(PipelineStepKind::Test);
    }
    if line.contains("validate") || contains_ascii_word(line, "check") {
        return Some(PipelineStepKind::Test);
    }
    if line.contains("bench") || line.contains("benchmark") || line.contains("eval") {
        return Some(PipelineStepKind::Eval);
    }
    if line.contains("lint")
        || line.contains("clippy")
        || line.contains("golangci-lint")
        || line.contains("eslint")
        || line.contains("ruff")
        || line.contains("mypy")
        || line.contains("pylint")
    {
        return Some(PipelineStepKind::Lint);
    }
    if line.contains("cargo fmt")
        || line.contains("rustfmt")
        || line.contains("gofmt")
        || line.contains("prettier")
        || line.contains(" fmt")
        || line.contains("format")
    {
        return Some(PipelineStepKind::Format);
    }
    None
}

fn contains_ascii_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    for (idx, _) in haystack.match_indices(needle) {
        let bytes = haystack.as_bytes();
        let before_ok = idx == 0 || !bytes[idx - 1].is_ascii_alphanumeric();
        let after_idx = idx.saturating_add(needle.len());
        let after_ok = after_idx >= bytes.len() || !bytes[after_idx].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

fn build_areas(
    _map_rows: &[(String, usize)],
    map_depth: usize,
    _artifact_store_file: Option<&str>,
    anchors: &[EmittedAnchor],
) -> Vec<EmittedArea> {
    // Keep this small: areas are the "map legend" for an agent, not a second directory listing.
    const MAX_AREAS: usize = 7;

    let mut out: Vec<EmittedArea> = Vec::new();
    let mut seen: HashSet<&'static str> = HashSet::new();

    let mut push_from_anchor =
        |anchor: &EmittedAnchor, kind: &'static str, label: &'static str, confidence: f32| {
            if !seen.insert(kind) {
                return;
            }
            out.push(EmittedArea {
                kind,
                label: label.to_string(),
                path: directory_key(&anchor.file, map_depth),
                confidence,
                ev_id: Some(anchor.ev_id.clone()),
            });
        };

    if let Some(anchor) = anchors.iter().find(|a| matches!(a.kind, AnchorKind::Canon)) {
        push_from_anchor(anchor, "docs", "Docs: canon", 0.84);
    }
    if let Some(anchor) = anchors.iter().find(|a| matches!(a.kind, AnchorKind::HowTo)) {
        push_from_anchor(anchor, "tooling", "Tooling: run / test", 0.83);
    }
    if let Some(anchor) = anchors.iter().find(|a| matches!(a.kind, AnchorKind::Ci)) {
        push_from_anchor(anchor, "ci", "CI: gates", 0.82);
    }
    if let Some(anchor) = anchors
        .iter()
        .find(|a| matches!(a.kind, AnchorKind::Entrypoint))
    {
        push_from_anchor(anchor, "core", "Core: code", 0.82);
    }
    if let Some(anchor) = anchors
        .iter()
        .find(|a| matches!(a.kind, AnchorKind::Contract))
    {
        push_from_anchor(anchor, "interfaces", "Interfaces: contracts", 0.8);
    }
    if let Some(anchor) = anchors
        .iter()
        .find(|a| matches!(a.kind, AnchorKind::Artifact))
    {
        push_from_anchor(anchor, "outputs", "Outputs: artifacts", 0.78);
    }
    if let Some(anchor) = anchors
        .iter()
        .find(|a| matches!(a.kind, AnchorKind::Experiment))
    {
        push_from_anchor(anchor, "experiments", "Experiments: baselines", 0.8);
    }
    if let Some(anchor) = anchors.iter().find(|a| matches!(a.kind, AnchorKind::Infra)) {
        push_from_anchor(anchor, "infra", "Infra: deploy", 0.8);
    }

    out.truncate(MAX_AREAS);
    out
}
