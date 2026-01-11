use anyhow::Result;
use context_indexer::{FileScanner, ToolMeta};
use context_protocol::{enforce_max_chars, BudgetTruncation, ToolNextAction};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use super::meaning_common::{
    build_ev_file_index, classify_boundaries, classify_files, contract_kind, detect_brokers,
    detect_channel_mentions, directory_key, extract_asyncapi_flows, hash_and_count_lines,
    infer_actor_by_path, infer_flow_actor, json_string, shrink_pack, BoundaryCandidate,
    BoundaryKind, BrokerCandidate, CognitivePack, EvidenceItem, EvidenceKind, FlowEdge,
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
const DEFAULT_MAX_BOUNDARIES: usize = 12;
const DEFAULT_EVIDENCE_END_LINE: usize = 120;

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
    files.sort();

    let mut dir_files: HashMap<String, usize> = HashMap::new();
    for rel in &files {
        let key = directory_key(rel, map_depth);
        *dir_files.entry(key).or_insert(0) += 1;
    }

    let mut map_rows = dir_files.into_iter().collect::<Vec<_>>();
    map_rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    map_rows.truncate(map_limit);

    let (entrypoints, contracts) = classify_files(&files);
    let mut boundaries = classify_boundaries(&files, &entrypoints, &contracts);
    boundaries.truncate(DEFAULT_MAX_BOUNDARIES);

    let flows = extract_asyncapi_flows(root, &contracts).await;

    let channels = flows.iter().map(|f| f.channel.clone()).collect::<Vec<_>>();
    let channel_mentions = detect_channel_mentions(root, &files, &channels).await;

    let brokers = detect_brokers(root, &files, &flows).await;

    let evidence = collect_evidence(
        root,
        &entrypoints,
        &contracts,
        &boundaries,
        &flows,
        &brokers,
    )
    .await;
    let ev_file_index = build_ev_file_index(&evidence);

    let root_fp = context_indexer::root_fingerprint(root_display);

    let mut cp = CognitivePack::new();
    cp.push_line("CPV1");
    cp.push_line(&format!("ROOT_FP {root_fp}"));
    cp.push_line(&format!("QUERY {}", json_string(&request.query)));

    let mut dict_paths: BTreeSet<String> = BTreeSet::new();
    for (path, _) in &map_rows {
        dict_paths.insert(path.clone());
    }
    for file in &entrypoints {
        dict_paths.insert(file.clone());
    }
    for file in &contracts {
        dict_paths.insert(file.clone());
    }
    for flow in &flows {
        dict_paths.insert(flow.channel.clone());
    }
    for broker in &brokers {
        dict_paths.insert(broker.file.clone());
    }
    for boundary in &boundaries {
        dict_paths.insert(boundary.file.clone());
    }
    for ev in &evidence {
        dict_paths.insert(ev.file.clone());
    }
    for path in dict_paths {
        cp.dict_intern(path);
    }

    cp.push_line("S MAP");
    for (path, files) in &map_rows {
        let d = cp.dict_id(path);
        cp.push_line(&format!("MAP path={d} files={files}"));
    }

    if !boundaries.is_empty() {
        cp.push_line("S BOUNDARIES");
        for boundary in &boundaries {
            let d = cp.dict_id(&boundary.file);
            let conf = format!("{:.2}", boundary.confidence.clamp(0.0, 1.0));
            let ev = ev_file_index
                .get(&boundary.file)
                .map(|id| format!(" ev={id}"))
                .unwrap_or_default();
            cp.push_line(&format!(
                "BOUNDARY kind={} file={d} conf={conf}{ev}",
                boundary.kind.as_str()
            ));
        }
    }

    if !entrypoints.is_empty() {
        cp.push_line("S ENTRYPOINTS");
        for file in &entrypoints {
            let d = cp.dict_id(file);
            let ev = ev_file_index
                .get(file)
                .map(|id| format!(" ev={id}"))
                .unwrap_or_default();
            cp.push_line(&format!("ENTRY file={d}{ev}"));
        }
    }

    if !contracts.is_empty() {
        cp.push_line("S CONTRACTS");
        for file in &contracts {
            let d = cp.dict_id(file);
            let ev = ev_file_index
                .get(file)
                .map(|id| format!(" ev={id}"))
                .unwrap_or_default();
            cp.push_line(&format!(
                "CONTRACT kind={} file={d}{ev}",
                contract_kind(file)
            ));
        }
    }

    if !flows.is_empty() {
        let mut flow_lines: Vec<String> = Vec::new();
        for flow in &flows {
            let contract_d = cp.dict_id(&flow.contract_file);
            let chan_d = cp.dict_id(&flow.channel);

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
            let actor_field = actor
                .as_deref()
                .map(|file| format!(" actor={}", cp.dict_id(file)))
                .unwrap_or_default();
            let conf = if actor.is_some() { actor_conf } else { 1.0 };

            let proto_field = flow
                .protocol
                .as_deref()
                .map(|p| format!(" proto={p}"))
                .unwrap_or_default();
            let ev_field = ev_file_index
                .get(&flow.contract_file)
                .or_else(|| actor.as_deref().and_then(|file| ev_file_index.get(file)))
                .map(|id| format!(" ev={id}"))
                .unwrap_or_default();
            if ev_field.is_empty() {
                continue;
            }
            flow_lines.push(format!(
                "FLOW contract={contract_d} chan={chan_d} dir={}{}{} conf={:.2}{}",
                flow.direction.as_str(),
                proto_field,
                actor_field,
                conf,
                ev_field
            ));
        }
        if !flow_lines.is_empty() {
            cp.push_line("S FLOWS");
            for line in &flow_lines {
                cp.push_line(line);
            }
        }
    }

    if !brokers.is_empty() {
        let mut broker_lines: Vec<String> = Vec::new();
        for broker in &brokers {
            let d = cp.dict_id(&broker.file);
            let conf = format!("{:.2}", broker.confidence.clamp(0.0, 1.0));
            let ev = ev_file_index
                .get(&broker.file)
                .map(|id| format!(" ev={id}"))
                .unwrap_or_default();
            if ev.is_empty() {
                continue;
            }
            broker_lines.push(format!(
                "BROKER proto={} file={d} conf={conf}{ev}",
                broker.proto
            ));
        }
        if !broker_lines.is_empty() {
            cp.push_line("S BROKERS");
            for line in &broker_lines {
                cp.push_line(line);
            }
        }
    }

    if !evidence.is_empty() {
        cp.push_line("S EVIDENCE");
        for (idx, ev) in evidence.iter().enumerate() {
            let ev_id = format!("ev{idx}");
            let d = cp.dict_id(&ev.file);
            let kind = match ev.kind {
                EvidenceKind::Entrypoint => "entrypoint".to_string(),
                EvidenceKind::Contract => "contract".to_string(),
                EvidenceKind::Boundary(kind) => format!("boundary.{}", kind.as_str()),
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
        .first()
        .map(|ev| {
            let ev_id = ev_file_index
                .get(ev.file.as_str())
                .map(|id| format!(" ev={id}"))
                .unwrap_or_default();
            let d = cp.dict_id(&ev.file);
            format!(
                "NBA evidence_fetch{ev_id} file={d} L{}-L{}",
                ev.start_line, ev.end_line,
            )
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
    entrypoints: &[String],
    contracts: &[String],
    boundaries: &[BoundaryCandidate],
    flows: &[FlowEdge],
    brokers: &[BrokerCandidate],
) -> Vec<EvidenceItem> {
    let mut candidates: Vec<(EvidenceKind, String)> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();

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
        out.push(EvidenceItem {
            kind,
            file: rel,
            start_line: 1,
            end_line: DEFAULT_EVIDENCE_END_LINE.min(lines.max(1)),
            source_hash: if hash.is_empty() { None } else { Some(hash) },
        });
    }
    out
}
