use anyhow::Result;
use context_indexer::{FileScanner, ToolMeta};
use context_protocol::{enforce_max_chars, BudgetTruncation, ToolNextAction};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use super::meaning_common::{
    artifact_scope_rank, build_ev_file_index, classify_boundaries, classify_files, contract_kind,
    detect_brokers, detect_channel_mentions, directory_key, extract_asyncapi_flows,
    hash_and_count_lines, infer_actor_by_path, infer_flow_actor, is_artifact_scope, json_string,
    read_file_prefix_utf8, shrink_pack, AnchorKind, BoundaryCandidate, BoundaryKind,
    BrokerCandidate, CognitivePack, EvidenceItem, EvidenceKind, FlowEdge,
};
use super::paths::normalize_relative_path;
use super::schemas::meaning_pack::{MeaningPackBudget, MeaningPackRequest, MeaningPackResult};
use super::schemas::response_mode::ResponseMode;
use super::secrets::is_potential_secret_path;

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

#[derive(Debug, Clone)]
struct AnchorCandidate {
    kind: AnchorKind,
    label: String,
    file: String,
    confidence: f32,
}

pub(super) async fn compute_meaning_pack_result(
    root: &Path,
    root_display: &str,
    request: &MeaningPackRequest,
) -> Result<MeaningPackResult> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let max_chars = request
        .max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);
    let map_depth = request.map_depth.unwrap_or(DEFAULT_MAP_DEPTH).clamp(1, 4);
    let map_limit = request.map_limit.unwrap_or(DEFAULT_MAP_LIMIT).clamp(1, 200);

    // v0: facts-only map derived from filesystem paths (gitignore-aware), no full-file parsing.
    let scanner = FileScanner::new(root);
    let mut files: Vec<String> = Vec::new();
    for abs in scanner.scan() {
        let Some(rel) = normalize_relative_path(root, &abs) else {
            continue;
        };
        if is_potential_secret_path(&rel) {
            continue;
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
        }
    }
    files.sort();
    files.dedup();

    let mut dir_files: HashMap<String, usize> = HashMap::new();
    let mut dir_files_with_artifacts: HashMap<String, usize> = HashMap::new();
    for rel in &files {
        let key = directory_key(rel, map_depth);
        *dir_files_with_artifacts.entry(key.clone()).or_insert(0) += 1;
        if is_artifact_scope(rel) {
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
    map_rows.truncate(map_limit);

    let (entrypoints, contracts) = classify_files(&files);
    let mut boundaries_full = classify_boundaries(&files, &entrypoints, &contracts);
    augment_k8s_manifest_boundaries(root, &files, &mut boundaries_full).await;
    let artifact_store_file = best_artifact_store_evidence_file(&files);
    let anchors = select_repo_anchors(
        &files,
        &entrypoints,
        &contracts,
        &boundaries_full,
        artifact_store_file.as_deref(),
    );
    boundaries_full.truncate(DEFAULT_MAX_BOUNDARIES);
    let boundaries = boundaries_full;

    let flows = extract_asyncapi_flows(root, &contracts).await;

    let channels = flows.iter().map(|f| f.channel.clone()).collect::<Vec<_>>();
    let channel_mentions = detect_channel_mentions(root, &files, &channels).await;

    let brokers = detect_brokers(root, &files, &flows).await;

    let evidence = collect_evidence(
        root,
        &anchors,
        &entrypoints,
        &contracts,
        &boundaries,
        &flows,
        &brokers,
    )
    .await;
    let ev_file_index = build_ev_file_index(&evidence);

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
    for file in &entrypoints {
        if emitted_entrypoints.len() >= DEFAULT_MAX_ENTRYPOINTS {
            break;
        }
        if !ev_file_index.contains_key(file) {
            continue;
        }
        emitted_entrypoints.push(file);
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

    #[derive(Clone)]
    struct EmittedAnchor {
        kind: AnchorKind,
        label: String,
        file: String,
        confidence: f32,
        ev_id: String,
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

    let mut used_ev_ids: HashSet<String> = HashSet::new();
    for anchor in &emitted_anchors {
        used_ev_ids.insert(anchor.ev_id.clone());
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

    cp.push_line("S MAP");
    for (path, files) in &map_rows {
        let d = cp.dict_id(path);
        cp.push_line(&format!("MAP path={d} files={files}"));
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

    let next_actions = if response_mode == ResponseMode::Full {
        build_next_actions(root_display, evidence.first())
    } else {
        Vec::new()
    };

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
        next_actions,
        meta: ToolMeta::default(),
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

fn build_next_actions(root_display: &str, first_ev: Option<&EvidenceItem>) -> Vec<ToolNextAction> {
    let Some(first_ev) = first_ev else {
        return Vec::new();
    };
    let reason_prefix = match first_ev.kind {
        EvidenceKind::Entrypoint => "Entrypoint",
        EvidenceKind::Contract => "Contract",
        EvidenceKind::Boundary(kind) => match kind {
            BoundaryKind::Cli => "CLI",
            BoundaryKind::Http => "HTTP",
            BoundaryKind::Env => "Env",
            BoundaryKind::Config => "Config",
            BoundaryKind::Db => "DB",
            BoundaryKind::Event => "Event",
        },
        EvidenceKind::Anchor(kind) => match kind {
            AnchorKind::Canon => "Canon",
            AnchorKind::HowTo => "HowTo",
            AnchorKind::Infra => "Infra",
            AnchorKind::Contract => "Contract",
            AnchorKind::Entrypoint => "Entrypoint",
            AnchorKind::Artifact => "Artifacts",
        },
    };
    vec![ToolNextAction {
        tool: "evidence_fetch".to_string(),
        args: serde_json::json!({
            "path": root_display,
            "items": [{
                "file": first_ev.file,
                "start_line": first_ev.start_line,
                "end_line": first_ev.end_line,
                "source_hash": first_ev.source_hash,
            }],
            "max_chars": 2000,
        }),
        reason: format!("{reason_prefix} evidence: fetch exact source lines (verbatim)."),
    }]
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

    out.truncate(DEFAULT_MAX_ANCHORS);
    out
}

fn best_artifact_store_evidence_file(files: &[String]) -> Option<String> {
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

fn best_canon_doc(files: &[String]) -> Option<String> {
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

fn best_howto_file(files: &[String]) -> Option<String> {
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
        } else if is_root
            && matches!(
                basename,
                "package.json" | "pyproject.toml" | "go.mod" | "cargo.toml"
            )
        {
            Some(1usize)
        } else if (lc.starts_with(".github/workflows/")
            && (lc.ends_with(".yml") || lc.ends_with(".yaml")))
            || (is_root && basename == ".gitlab-ci.yml")
        {
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

fn best_contract_file(contracts: &[String]) -> Option<String> {
    let mut candidates: Vec<(usize, &String)> = Vec::new();
    for file in contracts {
        let kind = contract_kind(file);
        let rank = match kind {
            "asyncapi" => 0usize,
            "openapi" => 1usize,
            "proto" => 2usize,
            "json_schema" => 3usize,
            _ => 4usize,
        };
        candidates.push((rank, file));
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    candidates.first().map(|(_, f)| (*f).clone())
}

fn best_infra_file(boundaries: &[BoundaryCandidate]) -> Option<String> {
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

async fn anchor_evidence_window(
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
