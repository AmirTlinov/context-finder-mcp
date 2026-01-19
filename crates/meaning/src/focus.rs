use anyhow::{anyhow, Context as AnyhowContext, Result};
use context_indexer::FileScanner;
use context_protocol::{enforce_max_chars, BudgetTruncation};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use crate::common::{
    build_ev_file_index, classify_boundaries, classify_files, contract_kind, detect_brokers,
    detect_channel_mentions, directory_key, evidence_fetch_payload_json, extract_asyncapi_flows,
    extract_code_outline, hash_and_count_lines, infer_actor_by_path, infer_flow_actor,
    is_artifact_scope, json_string, shrink_pack, AnchorKind, BoundaryCandidate, BoundaryKind,
    BrokerCandidate, CognitivePack, EvidenceItem, EvidenceKind, FlowEdge,
};
use crate::model::{MeaningFocusBudget, MeaningFocusRequest, MeaningFocusResult};
use crate::pack::{
    anchor_evidence_window, best_artifact_store_evidence_file, best_canon_doc, best_contract_file,
    best_experiment_file, best_howto_file, best_infra_file,
};
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

pub async fn meaning_focus(
    root: &Path,
    root_display: &str,
    request: &MeaningFocusRequest,
) -> Result<MeaningFocusResult> {
    let max_chars = request
        .max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);
    let map_depth = request.map_depth.unwrap_or(DEFAULT_MAP_DEPTH).clamp(1, 4);
    let map_limit = request.map_limit.unwrap_or(DEFAULT_MAP_LIMIT).clamp(1, 200);

    let focus_raw = request.focus.trim();
    if focus_raw.is_empty() {
        return Err(anyhow!("focus must not be empty"));
    }
    let focus_rel_in = focus_raw.replace('\\', "/");
    if is_potential_secret_path(&focus_rel_in) {
        return Err(anyhow!(
            "Refusing to focus on a potential secret path: '{focus_rel_in}'"
        ));
    }

    let canonical = root
        .join(Path::new(&focus_rel_in))
        .canonicalize()
        .with_context(|| format!("Failed to resolve focus path '{focus_rel_in}'"))?;
    if !canonical.starts_with(root) {
        return Err(anyhow!(
            "Focus path '{focus_rel_in}' is outside project root"
        ));
    }
    let focus_rel = normalize_relative_path(root, &canonical).unwrap_or(focus_rel_in);
    if is_potential_secret_path(&focus_rel) {
        return Err(anyhow!(
            "Refusing to focus on a potential secret path: '{focus_rel}'"
        ));
    }

    let focus_dir = if canonical.is_dir() {
        focus_rel.clone()
    } else {
        Path::new(&focus_rel)
            .parent()
            .and_then(|p| p.to_str())
            .map(|p| p.replace('\\', "/"))
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| ".".to_string())
    };
    let focus_prefix = if focus_dir == "." {
        None
    } else {
        Some(format!("{focus_dir}/"))
    };

    let outline = if canonical.is_dir() {
        Vec::new()
    } else {
        extract_code_outline(root, &focus_rel).await
    };

    let query = request
        .query
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(|q| q.to_string())
        .unwrap_or_else(|| format!("focus:{focus_rel}"));

    let scanner = FileScanner::new(root);
    let mut all_files: Vec<String> = Vec::new();
    for abs in scanner.scan() {
        let Some(rel) = normalize_relative_path(root, &abs) else {
            continue;
        };
        if is_potential_secret_path(&rel) {
            continue;
        }
        all_files.push(rel);
    }
    all_files.sort();

    let mut scope_files: Vec<String> = Vec::new();
    for rel in &all_files {
        let in_scope = match focus_prefix.as_deref() {
            Some(prefix) => rel.starts_with(prefix),
            None => true,
        };
        if in_scope {
            scope_files.push(rel.clone());
        }
    }
    let files_for_map = if scope_files.is_empty() {
        &all_files
    } else {
        &scope_files
    };

    let mut dir_files: HashMap<String, usize> = HashMap::new();
    let mut dir_files_with_artifacts: HashMap<String, usize> = HashMap::new();
    let focus_is_artifact = is_artifact_scope(&focus_rel) || is_artifact_scope(&focus_dir);
    for rel in files_for_map {
        let key = directory_key(rel, map_depth);
        *dir_files_with_artifacts.entry(key.clone()).or_insert(0) += 1;
        if !focus_is_artifact && is_artifact_scope(rel) {
            continue;
        }
        *dir_files.entry(key).or_insert(0) += 1;
    }
    let mut map_rows = dir_files.into_iter().collect::<Vec<_>>();
    map_rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if map_rows.is_empty() {
        map_rows = dir_files_with_artifacts.into_iter().collect::<Vec<_>>();
        map_rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    }
    map_rows.truncate(map_limit);

    let (entrypoints, contracts) = classify_files(files_for_map);
    let mut boundaries = classify_boundaries(files_for_map, &entrypoints, &contracts);
    boundaries.truncate(DEFAULT_MAX_BOUNDARIES);

    let artifact_store_file = best_artifact_store_evidence_file(files_for_map);
    let anchors = select_repo_anchors(
        files_for_map,
        &entrypoints,
        &contracts,
        &boundaries,
        artifact_store_file.as_deref(),
    );

    let flows = extract_asyncapi_flows(root, &contracts).await;

    let channels = flows.iter().map(|f| f.channel.clone()).collect::<Vec<_>>();
    let channel_mentions = detect_channel_mentions(root, files_for_map, &channels).await;

    let brokers = detect_brokers(root, files_for_map, &flows).await;

    let evidence = collect_focus_evidence(
        root,
        FocusEvidenceContext {
            focus_is_dir: canonical.is_dir(),
            focus_rel: &focus_rel,
            anchors: &anchors,
            entrypoints: &entrypoints,
            contracts: &contracts,
            boundaries: &boundaries,
            flows: &flows,
            brokers: &brokers,
        },
    )
    .await;
    let ev_file_index = build_ev_file_index(&evidence);

    let root_fp = context_indexer::root_fingerprint(root_display);

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

    let mut used_ev_ids: HashSet<String> = HashSet::new();
    if !evidence.is_empty() {
        used_ev_ids.insert("ev0".to_string());
    }
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
    dict_paths.insert(focus_dir.clone());
    dict_paths.insert(focus_rel.clone());
    for (path, _) in &map_rows {
        dict_paths.insert(path.clone());
    }
    for anchor in &emitted_anchors {
        dict_paths.insert(anchor.label.clone());
        dict_paths.insert(anchor.file.clone());
    }
    for symbol in &outline {
        dict_paths.insert(symbol.name.clone());
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
    cp.push_line(&format!("QUERY {}", json_string(&query)));

    for path in dict_paths {
        cp.dict_intern(path);
    }

    cp.push_line("S FOCUS");
    let d_dir = cp.dict_id(&focus_dir);
    let d_file = cp.dict_id(&focus_rel);
    cp.push_line(&format!("FOCUS dir={d_dir} file={d_file}"));

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

    if !outline.is_empty() {
        cp.push_line("S OUTLINE");
        for symbol in &outline {
            let d_name = cp.dict_id(&symbol.name);
            let conf = format!("{:.2}", symbol.confidence.clamp(0.0, 1.0));
            cp.push_line(&format!(
                "SYM kind={} name={d_name} file={d_file} L{}-L{} conf={conf}",
                symbol.kind, symbol.start_line, symbol.end_line
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
                // Keep NBA copy/paste runnable, but avoid blowing tight budgets by default.
                // Include source_hash only when max_chars is comfortably above the default.
                let source_hash = (max_chars >= 3_000)
                    .then_some(ev.source_hash.as_deref())
                    .flatten();
                let payload =
                    evidence_fetch_payload_json(&ev.file, ev.start_line, ev.end_line, source_hash);
                format!("NBA evidence_fetch {payload}")
            })
        })
        .unwrap_or_else(|| "NBA map".to_string());
    cp.push_line(&nba);

    let mut result = MeaningFocusResult {
        version: VERSION,
        query,
        format: "cpv1".to_string(),
        pack: cp.render(),
        budget: MeaningFocusBudget {
            max_chars,
            used_chars: 0,
            truncated: false,
            truncation: None,
        },
    };

    trim_to_budget(&mut result)?;
    Ok(result)
}

fn trim_to_budget(result: &mut MeaningFocusResult) -> anyhow::Result<()> {
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

struct FocusEvidenceContext<'a> {
    focus_is_dir: bool,
    focus_rel: &'a str,
    anchors: &'a [AnchorCandidate],
    entrypoints: &'a [String],
    contracts: &'a [String],
    boundaries: &'a [BoundaryCandidate],
    flows: &'a [FlowEdge],
    brokers: &'a [BrokerCandidate],
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

    out.truncate(DEFAULT_MAX_ANCHORS);
    out
}

async fn collect_focus_evidence(root: &Path, focus: FocusEvidenceContext<'_>) -> Vec<EvidenceItem> {
    let mut out: Vec<EvidenceItem> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();

    let focus_candidate = if focus.focus_is_dir {
        focus
            .anchors
            .first()
            .map(|a| a.file.clone())
            .or_else(|| focus.entrypoints.first().cloned())
            .or_else(|| focus.contracts.first().cloned())
            .or_else(|| focus.boundaries.first().map(|b| b.file.clone()))
    } else {
        Some(focus.focus_rel.to_string())
    };

    if let Some(rel) = focus_candidate.as_deref() {
        if seen.insert(rel) {
            let abs = root.join(rel);
            let (hash, lines) = hash_and_count_lines(&abs).await.ok().unwrap_or_default();
            let kind = if let Some(anchor) = focus.anchors.iter().find(|a| a.file == rel) {
                EvidenceKind::Anchor(anchor.kind)
            } else if focus.entrypoints.iter().any(|f| f == rel) {
                EvidenceKind::Entrypoint
            } else if focus.contracts.iter().any(|f| f == rel) {
                EvidenceKind::Contract
            } else if let Some(boundary) = focus.boundaries.iter().find(|b| b.file == rel) {
                EvidenceKind::Boundary(boundary.kind)
            } else {
                EvidenceKind::Boundary(BoundaryKind::Config)
            };
            let (start_line, end_line) = match kind {
                EvidenceKind::Anchor(anchor_kind) => {
                    let (start, end) =
                        anchor_evidence_window(root, rel, anchor_kind, DEFAULT_EVIDENCE_END_LINE)
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
                file: rel.to_string(),
                start_line,
                end_line,
                source_hash: if hash.is_empty() { None } else { Some(hash) },
            });
        }
    }

    let mut candidates: Vec<(EvidenceKind, String)> = Vec::new();

    for anchor in focus.anchors {
        if !seen.insert(anchor.file.as_str()) {
            continue;
        }
        candidates.push((EvidenceKind::Anchor(anchor.kind), anchor.file.clone()));
    }

    // Ensure event-driven claims have at least one evidence anchor (contract and/or actor).
    let mut must_contracts: Vec<String> = Vec::new();
    let mut must_entrypoints: Vec<String> = Vec::new();
    for flow in focus.flows {
        if must_contracts.len() < 2 && !must_contracts.iter().any(|c| c == &flow.contract_file) {
            must_contracts.push(flow.contract_file.clone());
        }
        if must_entrypoints.len() < 2 {
            if let Some(actor) = infer_flow_actor(&flow.contract_file, focus.entrypoints) {
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
    for broker in focus.brokers.iter().take(2) {
        if !seen.insert(broker.file.as_str()) {
            continue;
        }
        candidates.push((
            EvidenceKind::Boundary(BoundaryKind::Config),
            broker.file.clone(),
        ));
    }

    for file in focus.entrypoints.iter().take(DEFAULT_MAX_EVIDENCE) {
        if !seen.insert(file.as_str()) {
            continue;
        }
        candidates.push((EvidenceKind::Entrypoint, file.clone()));
    }
    for file in focus
        .contracts
        .iter()
        .take(DEFAULT_MAX_EVIDENCE.saturating_sub(candidates.len()))
    {
        if !seen.insert(file.as_str()) {
            continue;
        }
        candidates.push((EvidenceKind::Contract, file.clone()));
    }
    for boundary in focus
        .boundaries
        .iter()
        .take(DEFAULT_MAX_EVIDENCE.saturating_sub(candidates.len()))
    {
        if !seen.insert(boundary.file.as_str()) {
            continue;
        }
        candidates.push((EvidenceKind::Boundary(boundary.kind), boundary.file.clone()));
    }

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
    out.truncate(DEFAULT_MAX_EVIDENCE);
    out
}
