use crate::command::context::index_path;
use crate::command::context::load_store_mtime;
use crate::command::domain::{
    parse_payload, CommandOutcome, EvidenceFetchItem, EvidenceFetchOutput, EvidencePointer,
    MeaningFocusPayload, MeaningPackBudget, MeaningPackOutput, MeaningPackPayload,
    EVIDENCE_FETCH_VERSION, MEANING_PACK_VERSION,
};
use crate::command::warm;
use crate::command::{Hint, HintKind};
use anyhow::{anyhow, Context as AnyhowContext, Result};
use context_protocol::{enforce_max_chars, BudgetTruncation};
use context_vector_store::VectorStore;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

#[derive(Default)]
pub struct MeaningService;

impl MeaningService {
    pub async fn meaning_pack(
        &self,
        payload: serde_json::Value,
        ctx: &crate::command::context::CommandContext,
    ) -> Result<CommandOutcome> {
        const DEFAULT_MAX_CHARS: usize = 2_000;
        const MIN_MAX_CHARS: usize = 800;
        const MAX_MAX_CHARS: usize = 200_000;
        const MAP_DEPTH: usize = 2;
        const MAP_LIMIT: usize = 12;
        const MAX_EVIDENCE_ITEMS: usize = 12;
        const MAX_ANCHORS: usize = 7;
        const MAX_BOUNDARIES: usize = 12;
        const MAX_ENTRYPOINTS: usize = 8;
        const MAX_CONTRACTS: usize = 8;
        const MAX_FLOWS: usize = 12;
        const MAX_BROKERS: usize = 6;
        const DEFAULT_EVIDENCE_END_LINE: usize = 120;

        let payload: MeaningPackPayload = parse_payload(payload)?;
        let project_ctx = ctx.resolve_project(payload.project).await?;
        let _ = crate::heartbeat::ping(&project_ctx.root).await;
        let warm = warm::global_warmer().prewarm(&project_ctx.root).await;

        let max_chars = payload
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);

        let store_path = index_path(&project_ctx.root);
        crate::command::context::ensure_index_exists(&store_path)?;
        let store_mtime = load_store_mtime(&store_path).await.ok();
        let store = VectorStore::load(&store_path).await?;

        let mut all_files: BTreeSet<String> = BTreeSet::new();
        let mut dir_chunks: HashMap<String, usize> = HashMap::new();
        let mut dir_files: HashMap<String, BTreeSet<String>> = HashMap::new();
        for id in store.chunk_ids() {
            if let Some(chunk) = store.get_chunk(&id) {
                let file_path = chunk.chunk.file_path.clone();
                all_files.insert(file_path.clone());
                let key = dir_key(&file_path, MAP_DEPTH);
                *dir_chunks.entry(key.clone()).or_insert(0) += 1;
                dir_files.entry(key).or_default().insert(file_path);
            }
        }
        // Meaning mode benefits from some high-signal infra files that we intentionally treat as
        // \"noise\" for indexing/watch workflows (e.g. docker-compose). The index may not contain
        // them, so we add them back explicitly for broker/boundary extraction.
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
            if project_ctx.root.join(rel).is_file() {
                all_files.insert(rel.to_string());
            }
        }

        let mut map_rows = dir_chunks
            .iter()
            .map(|(path, chunks)| {
                let files = dir_files.get(path).map(|set| set.len()).unwrap_or(0);
                (path.clone(), files, *chunks)
            })
            .collect::<Vec<_>>();
        map_rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        map_rows.truncate(MAP_LIMIT);

        let files_vec = all_files.iter().cloned().collect::<Vec<_>>();
        let (entrypoints, contracts) = classify_files(&all_files);
        let mut boundaries_full = classify_boundaries(&all_files, &entrypoints, &contracts);
        augment_k8s_manifest_boundaries(&project_ctx.root, &files_vec, &mut boundaries_full).await;
        let anchors = select_repo_anchors(&files_vec, &entrypoints, &contracts, &boundaries_full);
        boundaries_full.truncate(MAX_BOUNDARIES);
        let boundaries = boundaries_full;

        let flows = extract_asyncapi_flows(&project_ctx.root, &contracts).await;
        let channels = flows.iter().map(|f| f.channel.clone()).collect::<Vec<_>>();
        let channel_mentions =
            detect_channel_mentions(&project_ctx.root, &files_vec, &channels).await;
        let brokers = detect_brokers(&project_ctx.root, &files_vec, &flows).await;

        let mut evidence_candidates: Vec<(EvidenceKind, String)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for anchor in anchors.iter().take(MAX_EVIDENCE_ITEMS) {
            if !seen.insert(anchor.file.clone()) {
                continue;
            }
            evidence_candidates.push((EvidenceKind::Anchor(anchor.kind), anchor.file.clone()));
        }

        // Ensure event-driven claims have at least one evidence anchor (contract and/or actor).
        let mut must_contracts: Vec<String> = Vec::new();
        let mut must_entrypoints: Vec<String> = Vec::new();
        for flow in &flows {
            if must_contracts.len() < 2 && !must_contracts.iter().any(|c| c == &flow.contract_file)
            {
                must_contracts.push(flow.contract_file.clone());
            }
            if must_entrypoints.len() < 2 {
                if let Some(actor) = infer_flow_actor(&flow.contract_file, &entrypoints) {
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
            if !seen.insert(file.clone()) {
                continue;
            }
            evidence_candidates.push((EvidenceKind::Contract, file.clone()));
        }
        for file in &must_entrypoints {
            if !seen.insert(file.clone()) {
                continue;
            }
            evidence_candidates.push((EvidenceKind::Entrypoint, file.clone()));
        }

        // Ensure broker config claims have evidence anchors.
        for broker in brokers.iter().take(2) {
            if !seen.insert(broker.file.clone()) {
                continue;
            }
            evidence_candidates.push((
                EvidenceKind::Boundary(BoundaryKind::Config),
                broker.file.clone(),
            ));
        }

        for file in entrypoints.iter().take(MAX_EVIDENCE_ITEMS) {
            if !seen.insert(file.clone()) {
                continue;
            }
            evidence_candidates.push((EvidenceKind::Entrypoint, file.clone()));
        }
        for file in contracts
            .iter()
            .take(MAX_EVIDENCE_ITEMS.saturating_sub(evidence_candidates.len()))
        {
            if !seen.insert(file.clone()) {
                continue;
            }
            evidence_candidates.push((EvidenceKind::Contract, file.clone()));
        }
        for boundary in boundaries
            .iter()
            .take(MAX_EVIDENCE_ITEMS.saturating_sub(evidence_candidates.len()))
        {
            if !seen.insert(boundary.file.clone()) {
                continue;
            }
            evidence_candidates
                .push((EvidenceKind::Boundary(boundary.kind), boundary.file.clone()));
        }

        let mut evidence: Vec<ComputedEvidence> = Vec::new();
        for (kind, rel) in evidence_candidates.into_iter().take(MAX_EVIDENCE_ITEMS) {
            let abs = project_ctx.root.join(&rel);
            let (hash, lines) = hash_and_count_lines(&abs).await.ok().unwrap_or_default();
            let (start_line, end_line) = match kind {
                EvidenceKind::Anchor(anchor_kind) => {
                    let (start, end) = anchor_evidence_window(
                        &project_ctx.root,
                        &rel,
                        anchor_kind,
                        DEFAULT_EVIDENCE_END_LINE,
                    )
                    .await;
                    let file_lines = lines.max(1);
                    let start = start.clamp(1, file_lines);
                    let end = end.clamp(start, file_lines);
                    (start, end)
                }
                _ => (1, DEFAULT_EVIDENCE_END_LINE.min(lines.max(1))),
            };
            evidence.push(ComputedEvidence {
                kind,
                file: rel,
                start_line,
                end_line,
                source_hash: if hash.is_empty() { None } else { Some(hash) },
            });
        }

        let root_display = project_ctx.root.display().to_string();
        let root_fp = context_indexer::root_fingerprint(&root_display);

        let mut ev_file_index: HashMap<String, String> = HashMap::new();
        for (idx, ev) in evidence.iter().enumerate() {
            ev_file_index
                .entry(ev.file.clone())
                .or_insert_with(|| format!("ev{idx}"));
        }

        let mut emitted_boundaries: Vec<&BoundaryCandidate> = Vec::new();
        for boundary in &boundaries {
            if emitted_boundaries.len() >= MAX_BOUNDARIES {
                break;
            }
            if !ev_file_index.contains_key(&boundary.file) {
                continue;
            }
            emitted_boundaries.push(boundary);
        }

        let mut emitted_entrypoints: Vec<&String> = Vec::new();
        for file in &entrypoints {
            if emitted_entrypoints.len() >= MAX_ENTRYPOINTS {
                break;
            }
            if !ev_file_index.contains_key(file) {
                continue;
            }
            emitted_entrypoints.push(file);
        }

        let mut emitted_contracts: Vec<&String> = Vec::new();
        for file in &contracts {
            if emitted_contracts.len() >= MAX_CONTRACTS {
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
            if emitted_flows.len() >= MAX_FLOWS {
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
            if emitted_brokers.len() >= MAX_BROKERS {
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
            if emitted_anchors.len() >= MAX_ANCHORS {
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
        for (path, _, _) in &map_rows {
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
        cp.push_line(&format!("QUERY {}", json_string(&payload.query)));

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
        for (path, files, _chunks) in &map_rows {
            let d = cp.dict_id(path);
            cp.push_line(&format!("MAP path={d} files={files}"));
        }

        if !emitted_boundaries.is_empty() {
            cp.push_line("S BOUNDARIES");
            for boundary in &emitted_boundaries {
                let d = cp.dict_id(&boundary.file);
                let conf = format!("{:.2}", boundary.confidence.clamp(0.0, 1.0));
                cp.push_line(&format!(
                    "BOUNDARY kind={} file={d} conf={conf} ev={}",
                    boundary.kind.as_str(),
                    ev_file_index
                        .get(&boundary.file)
                        .map(String::as_str)
                        .unwrap_or("ev0")
                ));
            }
        }

        if !emitted_entrypoints.is_empty() {
            cp.push_line("S ENTRYPOINTS");
            for file in &emitted_entrypoints {
                let d = cp.dict_id(file.as_str());
                let Some(ev_id) = ev_file_index.get(file.as_str()) else {
                    continue;
                };
                cp.push_line(&format!("ENTRY file={d} ev={ev_id}"));
            }
        }

        if !emitted_contracts.is_empty() {
            cp.push_line("S CONTRACTS");
            for file in &emitted_contracts {
                let d = cp.dict_id(file.as_str());
                let Some(ev_id) = ev_file_index.get(file.as_str()) else {
                    continue;
                };
                cp.push_line(&format!(
                    "CONTRACT kind={} file={d} ev={ev_id}",
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
                let kind = match ev.kind {
                    EvidenceKind::Entrypoint => "entrypoint".to_string(),
                    EvidenceKind::Contract => "contract".to_string(),
                    EvidenceKind::Boundary(kind) => format!("boundary.{}", kind.as_str()),
                    EvidenceKind::Anchor(kind) => format!("anchor.{}", kind.as_str()),
                };
                let d = cp.dict_id(&ev.file);
                let hash = ev
                    .source_hash
                    .as_deref()
                    .map(|h| format!(" sha256={h}"))
                    .unwrap_or_default();
                cp.push_line(&format!(
                    "EV {ev_id} kind={kind} file={d} L{}-L{}{}",
                    ev.start_line, ev.end_line, hash
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
                        ev.start_line, ev.end_line
                    )
                })
            })
            .unwrap_or_else(|| "NBA map".to_string());
        cp.push_line(&nba);

        let pack = cp.render();

        let mut output = MeaningPackOutput {
            version: MEANING_PACK_VERSION,
            query: payload.query,
            format: "cpv1".to_string(),
            pack,
            budget: MeaningPackBudget {
                max_chars,
                used_chars: 0,
                truncated: false,
                truncation: None,
            },
            next_actions: Vec::new(),
            meta: context_indexer::ToolMeta {
                index_state: None,
                root_fingerprint: Some(root_fp),
            },
        };

        enforce_max_chars(
            &mut output,
            max_chars,
            |inner, used| inner.budget.used_chars = used,
            |inner| {
                inner.budget.truncated = true;
                inner.budget.truncation = Some(BudgetTruncation::MaxChars);
            },
            |inner| shrink_pack(&mut inner.pack),
        )?;

        let mut outcome = CommandOutcome::from_value(output)?;
        outcome.meta.config_path = project_ctx.config_path;
        outcome.meta.index_updated = Some(false);
        outcome.meta.index_mtime_ms = store_mtime.map(crate::command::context::unix_ms);
        outcome.meta.index_size_bytes =
            tokio::fs::metadata(&store_path).await.ok().map(|m| m.len());
        outcome.meta.warm = Some(warm.warmed);
        outcome.meta.warm_cost_ms = Some(warm.warm_cost_ms);
        outcome.meta.warm_graph_cache_hit = Some(warm.graph_cache_hit);
        outcome.hints.extend(project_ctx.hints);
        outcome.hints.push(Hint {
            kind: HintKind::Info,
            text: "Meaning pack generated from existing index (facts-only v0)".to_string(),
        });
        Ok(outcome)
    }

    #[allow(dead_code)]
    pub async fn meaning_focus(
        &self,
        payload: serde_json::Value,
        ctx: &crate::command::context::CommandContext,
    ) -> Result<CommandOutcome> {
        const DEFAULT_MAX_CHARS: usize = 2_000;
        const MIN_MAX_CHARS: usize = 800;
        const MAX_MAX_CHARS: usize = 200_000;
        const MAP_DEPTH: usize = 2;
        const MAP_LIMIT: usize = 12;
        const MAX_EVIDENCE_ITEMS: usize = 12;
        const MAX_BOUNDARIES: usize = 12;
        const MAX_ENTRYPOINTS: usize = 8;
        const MAX_CONTRACTS: usize = 8;
        const MAX_FLOWS: usize = 12;
        const MAX_BROKERS: usize = 6;
        const DEFAULT_EVIDENCE_END_LINE: usize = 120;

        let payload: MeaningFocusPayload = parse_payload(payload)?;
        let project_ctx = ctx.resolve_project(payload.project).await?;
        let _ = crate::heartbeat::ping(&project_ctx.root).await;
        let warm = warm::global_warmer().prewarm(&project_ctx.root).await;

        let max_chars = payload
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);

        let store_path = index_path(&project_ctx.root);
        crate::command::context::ensure_index_exists(&store_path)?;
        let store_mtime = load_store_mtime(&store_path).await.ok();
        let store = VectorStore::load(&store_path).await?;

        let focus_raw = payload.focus.trim();
        if focus_raw.is_empty() {
            return Err(anyhow!("focus must not be empty"));
        }
        let focus_rel = focus_raw.replace('\\', "/");

        let canonical = project_ctx
            .root
            .join(Path::new(&focus_rel))
            .canonicalize()
            .with_context(|| format!("Failed to resolve focus path '{focus_rel}'"))?;
        if !canonical.starts_with(&project_ctx.root) {
            return Err(anyhow!("Focus path '{focus_rel}' is outside project root"));
        }
        let focus_rel = normalize_relative_path(&project_ctx.root, &canonical).unwrap_or(focus_rel);

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
            extract_code_outline(&project_ctx.root, &focus_rel).await
        };

        let query = payload
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(|q| q.to_string())
            .unwrap_or_else(|| format!("focus:{focus_rel}"));

        let mut all_files: BTreeSet<String> = BTreeSet::new();
        let mut scope_files: BTreeSet<String> = BTreeSet::new();
        let mut dir_chunks: HashMap<String, usize> = HashMap::new();
        let mut dir_files: HashMap<String, BTreeSet<String>> = HashMap::new();
        for id in store.chunk_ids() {
            if let Some(chunk) = store.get_chunk(&id) {
                let file_path = chunk.chunk.file_path.clone();
                all_files.insert(file_path.clone());
                let in_scope = match focus_prefix.as_deref() {
                    Some(prefix) => file_path.starts_with(prefix),
                    None => true,
                };
                if !in_scope {
                    continue;
                }
                scope_files.insert(file_path.clone());
                let key = dir_key(&file_path, MAP_DEPTH);
                *dir_chunks.entry(key.clone()).or_insert(0) += 1;
                dir_files.entry(key).or_default().insert(file_path);
            }
        }

        let mut map_rows = dir_chunks
            .iter()
            .map(|(path, chunks)| {
                let files = dir_files.get(path).map(|set| set.len()).unwrap_or(0);
                (path.clone(), files, *chunks)
            })
            .collect::<Vec<_>>();
        map_rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        map_rows.truncate(MAP_LIMIT);

        let (entrypoints, contracts) = if scope_files.is_empty() {
            classify_files(&all_files)
        } else {
            classify_files(&scope_files)
        };
        let mut boundaries = if scope_files.is_empty() {
            classify_boundaries(&all_files, &entrypoints, &contracts)
        } else {
            classify_boundaries(&scope_files, &entrypoints, &contracts)
        };
        boundaries.truncate(MAX_BOUNDARIES);

        let flows = extract_asyncapi_flows(&project_ctx.root, &contracts).await;
        let files_vec = if scope_files.is_empty() {
            all_files.iter().cloned().collect::<Vec<_>>()
        } else {
            scope_files.iter().cloned().collect::<Vec<_>>()
        };
        let channels = flows.iter().map(|f| f.channel.clone()).collect::<Vec<_>>();
        let channel_mentions =
            detect_channel_mentions(&project_ctx.root, &files_vec, &channels).await;
        let brokers = detect_brokers(&project_ctx.root, &files_vec, &flows).await;

        let mut evidence: Vec<ComputedEvidence> = Vec::new();
        {
            let abs = project_ctx.root.join(&focus_rel);
            let (hash, lines) = hash_and_count_lines(&abs).await.ok().unwrap_or_default();
            let kind = if contracts.iter().any(|c| c == &focus_rel) {
                EvidenceKind::Contract
            } else if entrypoints.iter().any(|e| e == &focus_rel) {
                EvidenceKind::Entrypoint
            } else {
                EvidenceKind::Boundary(BoundaryKind::Config)
            };
            evidence.push(ComputedEvidence {
                kind,
                file: focus_rel.clone(),
                start_line: 1,
                end_line: DEFAULT_EVIDENCE_END_LINE.min(lines.max(1)),
                source_hash: if hash.is_empty() { None } else { Some(hash) },
            });
        }

        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(focus_rel.clone());
        let mut evidence_candidates: Vec<(EvidenceKind, String)> = Vec::new();

        // Ensure event-driven claims have at least one evidence anchor (contract and/or actor).
        let mut must_contracts: Vec<String> = Vec::new();
        let mut must_entrypoints: Vec<String> = Vec::new();
        for flow in &flows {
            if must_contracts.len() < 2 && !must_contracts.iter().any(|c| c == &flow.contract_file)
            {
                must_contracts.push(flow.contract_file.clone());
            }
            if must_entrypoints.len() < 2 {
                if let Some(actor) = infer_flow_actor(&flow.contract_file, &entrypoints) {
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
            if !seen.insert(file.clone()) {
                continue;
            }
            evidence_candidates.push((EvidenceKind::Contract, file.clone()));
        }
        for file in &must_entrypoints {
            if !seen.insert(file.clone()) {
                continue;
            }
            evidence_candidates.push((EvidenceKind::Entrypoint, file.clone()));
        }

        // Ensure broker config claims have evidence anchors.
        for broker in brokers.iter().take(2) {
            if !seen.insert(broker.file.clone()) {
                continue;
            }
            evidence_candidates.push((
                EvidenceKind::Boundary(BoundaryKind::Config),
                broker.file.clone(),
            ));
        }

        for file in entrypoints.iter().take(MAX_EVIDENCE_ITEMS) {
            if !seen.insert(file.clone()) {
                continue;
            }
            evidence_candidates.push((EvidenceKind::Entrypoint, file.clone()));
        }
        for file in contracts
            .iter()
            .take(MAX_EVIDENCE_ITEMS.saturating_sub(evidence_candidates.len()))
        {
            if !seen.insert(file.clone()) {
                continue;
            }
            evidence_candidates.push((EvidenceKind::Contract, file.clone()));
        }
        for boundary in boundaries
            .iter()
            .take(MAX_EVIDENCE_ITEMS.saturating_sub(evidence_candidates.len()))
        {
            if !seen.insert(boundary.file.clone()) {
                continue;
            }
            evidence_candidates
                .push((EvidenceKind::Boundary(boundary.kind), boundary.file.clone()));
        }

        for (kind, rel) in evidence_candidates.into_iter().take(MAX_EVIDENCE_ITEMS) {
            let abs = project_ctx.root.join(&rel);
            let (hash, lines) = hash_and_count_lines(&abs).await.ok().unwrap_or_default();
            evidence.push(ComputedEvidence {
                kind,
                file: rel,
                start_line: 1,
                end_line: DEFAULT_EVIDENCE_END_LINE.min(lines.max(1)),
                source_hash: if hash.is_empty() { None } else { Some(hash) },
            });
        }

        let root_display = project_ctx.root.display().to_string();
        let root_fp = context_indexer::root_fingerprint(&root_display);

        let mut ev_file_index: HashMap<String, String> = HashMap::new();
        for (idx, ev) in evidence.iter().enumerate() {
            ev_file_index
                .entry(ev.file.clone())
                .or_insert_with(|| format!("ev{idx}"));
        }

        let mut emitted_boundaries: Vec<&BoundaryCandidate> = Vec::new();
        for boundary in &boundaries {
            if emitted_boundaries.len() >= MAX_BOUNDARIES {
                break;
            }
            if !ev_file_index.contains_key(&boundary.file) {
                continue;
            }
            emitted_boundaries.push(boundary);
        }

        let mut emitted_entrypoints: Vec<&String> = Vec::new();
        for file in &entrypoints {
            if emitted_entrypoints.len() >= MAX_ENTRYPOINTS {
                break;
            }
            if !ev_file_index.contains_key(file) {
                continue;
            }
            emitted_entrypoints.push(file);
        }

        let mut emitted_contracts: Vec<&String> = Vec::new();
        for file in &contracts {
            if emitted_contracts.len() >= MAX_CONTRACTS {
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
            if emitted_flows.len() >= MAX_FLOWS {
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
            if emitted_brokers.len() >= MAX_BROKERS {
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

        let mut dict_paths = BTreeSet::new();
        dict_paths.insert(focus_dir.clone());
        dict_paths.insert(focus_rel.clone());
        for (path, _, _) in &map_rows {
            dict_paths.insert(path.clone());
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
        for (path, files, _chunks) in &map_rows {
            let d = cp.dict_id(path);
            cp.push_line(&format!("MAP path={d} files={files}"));
        }

        if !emitted_boundaries.is_empty() {
            cp.push_line("S BOUNDARIES");
            for boundary in &emitted_boundaries {
                let d = cp.dict_id(&boundary.file);
                let conf = format!("{:.2}", boundary.confidence.clamp(0.0, 1.0));
                cp.push_line(&format!(
                    "BOUNDARY kind={} file={d} conf={conf} ev={}",
                    boundary.kind.as_str(),
                    ev_file_index
                        .get(&boundary.file)
                        .map(String::as_str)
                        .unwrap_or("ev0")
                ));
            }
        }

        if !emitted_entrypoints.is_empty() {
            cp.push_line("S ENTRYPOINTS");
            for file in &emitted_entrypoints {
                let d = cp.dict_id(file.as_str());
                let Some(ev_id) = ev_file_index.get(file.as_str()) else {
                    continue;
                };
                cp.push_line(&format!("ENTRY file={d} ev={ev_id}"));
            }
        }

        if !emitted_contracts.is_empty() {
            cp.push_line("S CONTRACTS");
            for file in &emitted_contracts {
                let d = cp.dict_id(file.as_str());
                let Some(ev_id) = ev_file_index.get(file.as_str()) else {
                    continue;
                };
                cp.push_line(&format!(
                    "CONTRACT kind={} file={d} ev={ev_id}",
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
                let kind = match ev.kind {
                    EvidenceKind::Entrypoint => "entrypoint".to_string(),
                    EvidenceKind::Contract => "contract".to_string(),
                    EvidenceKind::Boundary(kind) => format!("boundary.{}", kind.as_str()),
                    EvidenceKind::Anchor(kind) => format!("anchor.{}", kind.as_str()),
                };
                let d = cp.dict_id(&ev.file);
                let hash = ev
                    .source_hash
                    .as_deref()
                    .map(|h| format!(" sha256={h}"))
                    .unwrap_or_default();
                cp.push_line(&format!(
                    "EV {ev_id} kind={kind} file={d} L{}-L{}{}",
                    ev.start_line, ev.end_line, hash
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
                        ev.start_line, ev.end_line
                    )
                })
            })
            .unwrap_or_else(|| "NBA map".to_string());
        cp.push_line(&nba);

        let pack = cp.render();

        let mut output = MeaningPackOutput {
            version: MEANING_PACK_VERSION,
            query,
            format: "cpv1".to_string(),
            pack,
            budget: MeaningPackBudget {
                max_chars,
                used_chars: 0,
                truncated: false,
                truncation: None,
            },
            next_actions: Vec::new(),
            meta: context_indexer::ToolMeta {
                index_state: None,
                root_fingerprint: Some(root_fp),
            },
        };

        enforce_max_chars(
            &mut output,
            max_chars,
            |inner, used| inner.budget.used_chars = used,
            |inner| {
                inner.budget.truncated = true;
                inner.budget.truncation = Some(BudgetTruncation::MaxChars);
            },
            |inner| shrink_pack(&mut inner.pack),
        )?;

        let mut outcome = CommandOutcome::from_value(output)?;
        outcome.meta.config_path = project_ctx.config_path;
        outcome.meta.index_updated = Some(false);
        outcome.meta.index_mtime_ms = store_mtime.map(crate::command::context::unix_ms);
        outcome.meta.index_size_bytes =
            tokio::fs::metadata(&store_path).await.ok().map(|m| m.len());
        outcome.meta.warm = Some(warm.warmed);
        outcome.meta.warm_cost_ms = Some(warm.warm_cost_ms);
        outcome.meta.warm_graph_cache_hit = Some(warm.graph_cache_hit);
        outcome.hints.extend(project_ctx.hints);
        outcome.hints.push(Hint {
            kind: HintKind::Info,
            text: "Meaning focus generated from existing index (facts-only v0)".to_string(),
        });
        Ok(outcome)
    }

    pub async fn evidence_fetch(
        &self,
        payload: serde_json::Value,
        ctx: &crate::command::context::CommandContext,
    ) -> Result<CommandOutcome> {
        const DEFAULT_MAX_CHARS: usize = 8_000;
        const MIN_MAX_CHARS: usize = 800;
        const MAX_MAX_CHARS: usize = 200_000;
        const DEFAULT_MAX_LINES: usize = 200;
        const MAX_MAX_LINES: usize = 5_000;

        let payload: crate::command::domain::EvidenceFetchPayload = parse_payload(payload)?;
        let project_ctx = ctx.resolve_project(payload.project).await?;
        let _ = crate::heartbeat::ping(&project_ctx.root).await;
        let warm = warm::global_warmer().prewarm(&project_ctx.root).await;

        let max_chars = payload
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);
        let max_lines = payload
            .max_lines
            .unwrap_or(DEFAULT_MAX_LINES)
            .clamp(1, MAX_MAX_LINES);
        let strict_hash = payload.strict_hash.unwrap_or(false);

        let root_display = project_ctx.root.display().to_string();
        let root_fp = context_indexer::root_fingerprint(&root_display);

        let mut items = Vec::new();
        for mut evidence in payload.items {
            let rel = evidence.file.trim();
            if rel.is_empty() {
                return Err(anyhow!("Evidence file path must not be empty"));
            }
            let rel = rel.replace('\\', "/");

            let canonical = project_ctx
                .root
                .join(Path::new(&rel))
                .canonicalize()
                .with_context(|| format!("Failed to resolve evidence path '{rel}'"))?;
            if !canonical.starts_with(&project_ctx.root) {
                return Err(anyhow!("Evidence file '{rel}' is outside project root"));
            }
            let display_rel = normalize_relative_path(&project_ctx.root, &canonical).unwrap_or(rel);

            let (hash, file_lines) = hash_and_count_lines(&canonical).await?;
            let stale = evidence
                .source_hash
                .as_deref()
                .map(|expected| expected != hash)
                .unwrap_or(false);
            if stale && strict_hash {
                return Err(anyhow!(
                    "Evidence source_hash mismatch for {display_rel} (expected={}, got={hash})",
                    evidence.source_hash.as_deref().unwrap_or("<missing>")
                ));
            }

            evidence.file = display_rel.clone();
            evidence.source_hash = Some(hash);

            let start_line = evidence.start_line.max(1);
            let end_line = evidence.end_line.max(start_line).min(file_lines.max(1));
            let (content, truncated) =
                read_file_lines_window(&canonical, start_line, end_line, max_lines).await?;

            items.push(EvidenceFetchItem {
                evidence: EvidencePointer {
                    start_line,
                    end_line,
                    ..evidence
                },
                content,
                truncated,
                stale,
            });
        }

        let mut output = EvidenceFetchOutput {
            version: EVIDENCE_FETCH_VERSION,
            items,
            budget: crate::command::domain::EvidenceFetchBudget {
                max_chars,
                used_chars: 0,
                truncated: false,
                truncation: None,
            },
            next_actions: Vec::new(),
            meta: context_indexer::ToolMeta {
                index_state: None,
                root_fingerprint: Some(root_fp),
            },
        };

        enforce_max_chars(
            &mut output,
            max_chars,
            |inner, used| inner.budget.used_chars = used,
            |inner| {
                inner.budget.truncated = true;
                inner.budget.truncation = Some(BudgetTruncation::MaxChars);
            },
            |inner| {
                if inner.items.len() > 1 {
                    inner.items.pop();
                    return true;
                }
                if let Some(item) = inner.items.first_mut() {
                    if item.content.is_empty() {
                        return false;
                    }
                    item.truncated = true;
                    let target = item.content.chars().count().saturating_sub(200);
                    item.content = item.content.chars().take(target).collect::<String>();
                    return true;
                }
                false
            },
        )?;

        let mut outcome = CommandOutcome::from_value(output)?;
        outcome.meta.config_path = project_ctx.config_path;
        outcome.meta.index_updated = Some(false);
        outcome.meta.warm = Some(warm.warmed);
        outcome.meta.warm_cost_ms = Some(warm.warm_cost_ms);
        outcome.meta.warm_graph_cache_hit = Some(warm.graph_cache_hit);
        outcome.hints.extend(project_ctx.hints);
        Ok(outcome)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundaryKind {
    Cli,
    Http,
    Env,
    Config,
    Db,
    Event,
}

impl BoundaryKind {
    fn as_str(self) -> &'static str {
        match self {
            BoundaryKind::Cli => "cli",
            BoundaryKind::Http => "http",
            BoundaryKind::Env => "env",
            BoundaryKind::Config => "config",
            BoundaryKind::Db => "db",
            BoundaryKind::Event => "event",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnchorKind {
    Canon,
    HowTo,
    Infra,
    Contract,
    Entrypoint,
}

impl AnchorKind {
    fn as_str(self) -> &'static str {
        match self {
            AnchorKind::Canon => "canon",
            AnchorKind::HowTo => "howto",
            AnchorKind::Infra => "infra",
            AnchorKind::Contract => "contract",
            AnchorKind::Entrypoint => "entrypoint",
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
struct BoundaryCandidate {
    kind: BoundaryKind,
    file: String,
    confidence: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvidenceKind {
    Entrypoint,
    Contract,
    Boundary(BoundaryKind),
    Anchor(AnchorKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlowDirection {
    Publish,
    Subscribe,
}

impl FlowDirection {
    fn as_str(self) -> &'static str {
        match self {
            FlowDirection::Publish => "pub",
            FlowDirection::Subscribe => "sub",
        }
    }
}

#[derive(Debug, Clone)]
struct FlowEdge {
    contract_file: String,
    channel: String,
    direction: FlowDirection,
    protocol: Option<String>,
}

#[derive(Debug, Default)]
struct AsyncApiSummary {
    protocols: Vec<String>,
    channels: Vec<AsyncApiChannel>,
}

#[derive(Debug, Default)]
struct AsyncApiChannel {
    name: String,
    publish: bool,
    subscribe: bool,
}

#[derive(Debug)]
struct ComputedEvidence {
    kind: EvidenceKind,
    file: String,
    start_line: usize,
    end_line: usize,
    source_hash: Option<String>,
}

fn dir_key(file_path: &str, depth: usize) -> String {
    let parts: Vec<&str> = file_path.split('/').collect();
    parts
        .iter()
        .take(depth.max(1))
        .cloned()
        .collect::<Vec<_>>()
        .join("/")
}

fn classify_files(all_files: &BTreeSet<String>) -> (Vec<String>, Vec<String>) {
    let mut entrypoints = Vec::new();
    let mut contracts = Vec::new();

    for file in all_files {
        let file_lc = file.to_ascii_lowercase();
        if is_entrypoint_candidate(&file_lc) {
            entrypoints.push(file.clone());
            continue;
        }
        if is_contract_candidate(&file_lc) {
            contracts.push(file.clone());
        }
    }

    entrypoints.sort();
    contracts.sort();
    (entrypoints, contracts)
}

fn classify_boundaries(
    all_files: &BTreeSet<String>,
    entrypoints: &[String],
    contracts: &[String],
) -> Vec<BoundaryCandidate> {
    let mut out: Vec<BoundaryCandidate> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for file in all_files {
        let lc = file.to_ascii_lowercase();
        let kind = match lc.as_str() {
            "cargo.toml" | "package.json" | "pyproject.toml" | "go.mod" | "pom.xml"
            | "build.gradle" | "build.gradle.kts" | "makefile" | "justfile" => {
                Some(BoundaryKind::Config)
            }
            ".env.example" | ".env.sample" | ".env.template" | ".env.dist" => {
                Some(BoundaryKind::Env)
            }
            ".github/workflows/ci.yml" | ".github/workflows/ci.yaml" => Some(BoundaryKind::Config),
            _ => None,
        };
        let Some(kind) = kind else { continue };
        if !seen.insert(file.clone()) {
            continue;
        }
        out.push(BoundaryCandidate {
            kind,
            file: file.clone(),
            confidence: 1.0,
        });
    }

    for file in entrypoints {
        if !seen.insert(file.clone()) {
            continue;
        }
        let lc = file.to_ascii_lowercase();
        let (kind, confidence) =
            if lc.contains("/server.") || lc.contains("/api/") || lc.contains("/http/") {
                (BoundaryKind::Http, 0.7)
            } else if lc.contains("/cli/") || lc.contains("/cmd/") || lc.contains("/bin/") {
                (BoundaryKind::Cli, 0.7)
            } else {
                (BoundaryKind::Cli, 0.55)
            };
        out.push(BoundaryCandidate {
            kind,
            file: file.clone(),
            confidence,
        });
    }

    if contracts.iter().any(|file| {
        let lc = file.to_ascii_lowercase();
        lc.ends_with("openapi.json")
            || lc.ends_with("openapi.yaml")
            || lc.ends_with("openapi.yml")
            || lc.contains("/openapi.")
    }) {
        if let Some(server) = entrypoints.iter().find(|file| {
            let lc = file.to_ascii_lowercase();
            lc.contains("server") || lc.contains("app")
        }) {
            if seen.insert(server.clone()) {
                out.push(BoundaryCandidate {
                    kind: BoundaryKind::Http,
                    file: server.clone(),
                    confidence: 0.65,
                });
            }
        }
    }

    // Infra boundary: highlight k8s/helm/terraform layouts (path-only, safe).
    //
    // Keep this tight to avoid flooding boundaries for repos with many manifests.
    let mut infra_candidates: Vec<(usize, &String, f32)> = Vec::new();
    for file in all_files {
        let lc = file.to_ascii_lowercase();
        let basename = lc.rsplit('/').next().unwrap_or(lc.as_str());
        let is_yaml = lc.ends_with(".yaml") || lc.ends_with(".yml");
        let is_tf = lc.ends_with(".tf") || lc.ends_with(".tfvars") || lc.ends_with(".hcl");
        let is_tiltfile = basename == "tiltfile" && lc == "tiltfile";

        let is_k8s_dir = lc.starts_with("k8s/")
            || lc.contains("/k8s/")
            || lc.starts_with("kubernetes/")
            || lc.contains("/kubernetes/")
            || lc.starts_with("manifests/")
            || lc.contains("/manifests/")
            || lc.starts_with("deploy/")
            || lc.contains("/deploy/")
            || lc.starts_with("kustomize/")
            || lc.contains("/kustomize/");
        let is_helm_dir = lc.starts_with("charts/")
            || lc.contains("/charts/")
            || lc.contains("/helm/")
            || basename == "chart.yaml"
            || basename == "values.yaml"
            || basename == "values.yml"
            || basename == "helmfile.yaml"
            || basename == "helmfile.yml"
            || basename == "helmrelease.yaml"
            || basename == "helmrelease.yml";
        let is_gitops_dir = lc.starts_with("argocd/")
            || lc.contains("/argocd/")
            || lc.starts_with("argo/")
            || lc.contains("/argo/")
            || lc.starts_with("flux/")
            || lc.contains("/flux/")
            || lc.starts_with("gitops/")
            || lc.contains("/gitops/")
            || lc.starts_with("clusters/")
            || lc.contains("/clusters/");
        let is_tf_dir = lc.starts_with("terraform/")
            || lc.contains("/terraform/")
            || lc.starts_with("infra/")
            || lc.contains("/infra/");
        let is_tf_root_candidate = matches!(
            basename,
            "main.tf"
                | "variables.tf"
                | "versions.tf"
                | "provider.tf"
                | "providers.tf"
                | "backend.tf"
                | "outputs.tf"
                | "terraform.tf"
                | "terragrunt.hcl"
        );
        let is_infra_yaml = is_k8s_dir
            || is_helm_dir
            || is_gitops_dir
            || matches!(
                basename,
                "chart.yaml"
                    | "values.yaml"
                    | "values.yml"
                    | "helmfile.yaml"
                    | "helmfile.yml"
                    | "helmrelease.yaml"
                    | "helmrelease.yml"
                    | "kustomization.yaml"
                    | "kustomization.yml"
                    | "skaffold.yaml"
                    | "skaffold.yml"
                    | "werf.yaml"
                    | "werf.yml"
                    | "devspace.yaml"
                    | "devspace.yml"
            );

        if !(is_yaml || is_tf || is_tiltfile) {
            continue;
        }
        if is_yaml && !is_infra_yaml {
            continue;
        }
        if is_tf && !(is_tf_dir || is_tf_root_candidate || basename == "terragrunt.hcl") {
            continue;
        }

        let (rank, confidence) = if basename == "chart.yaml" {
            (0usize, 0.9f32)
        } else if basename == "values.yaml" || basename == "values.yml" {
            (1usize, 0.85f32)
        } else if basename == "helmfile.yaml" || basename == "helmfile.yml" {
            (2usize, 0.82f32)
        } else if basename == "helmrelease.yaml" || basename == "helmrelease.yml" {
            (3usize, 0.83f32)
        } else if basename == "kustomization.yaml" || basename == "kustomization.yml" {
            (4usize, 0.85f32)
        } else if is_gitops_dir
            && matches!(
                basename,
                "application.yaml"
                    | "application.yml"
                    | "applicationset.yaml"
                    | "applicationset.yml"
            )
        {
            (5usize, 0.82f32)
        } else if basename == "terragrunt.hcl" {
            (6usize, 0.85f32)
        } else if basename == "skaffold.yaml" || basename == "skaffold.yml" {
            (7usize, 0.83f32)
        } else if is_tiltfile {
            (8usize, 0.83f32)
        } else if basename == "werf.yaml" || basename == "werf.yml" {
            (9usize, 0.82f32)
        } else if basename == "devspace.yaml" || basename == "devspace.yml" {
            (10usize, 0.82f32)
        } else if lc.contains("ingress") {
            (11usize, 0.8f32)
        } else if lc.contains("service") {
            (12usize, 0.78f32)
        } else if lc.contains("deployment") || lc.contains("statefulset") {
            (13usize, 0.78f32)
        } else if basename == "main.tf" {
            (14usize, 0.85f32)
        } else if basename == "variables.tf" {
            (15usize, 0.82f32)
        } else if matches!(
            basename,
            "versions.tf"
                | "provider.tf"
                | "providers.tf"
                | "backend.tf"
                | "outputs.tf"
                | "terraform.tf"
        ) {
            (16usize, 0.8f32)
        } else if is_tf {
            (17usize, 0.78f32)
        } else {
            (18usize, 0.75f32)
        };

        infra_candidates.push((rank, file, confidence));
    }
    infra_candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    infra_candidates.truncate(8);
    for (_, file, confidence) in infra_candidates {
        if !seen.insert(file.clone()) {
            continue;
        }
        out.push(BoundaryCandidate {
            kind: BoundaryKind::Config,
            file: file.clone(),
            confidence,
        });
    }

    for file in all_files {
        let lc = file.to_ascii_lowercase();
        let is_db = lc.starts_with("migrations/")
            || lc.contains("/migrations/")
            || lc.ends_with("schema.sql")
            || lc.ends_with("schema.prisma")
            || lc.starts_with("prisma/");
        if !is_db {
            continue;
        }
        if !seen.insert(file.clone()) {
            continue;
        }
        out.push(BoundaryCandidate {
            kind: BoundaryKind::Db,
            file: file.clone(),
            confidence: 0.85,
        });
    }

    // Event boundary: AsyncAPI and schema-like assets for message-driven systems.
    for file in all_files {
        let lc = file.to_ascii_lowercase();
        let is_event = lc == "asyncapi.yaml"
            || lc == "asyncapi.yml"
            || lc == "asyncapi.json"
            || lc.contains("/asyncapi.")
            || lc.ends_with(".avsc")
            || lc.starts_with("events/")
            || lc.contains("/events/")
            || lc.starts_with("schemas/events/")
            || lc.contains("/schemas/events/")
            || lc.starts_with("messages/")
            || lc.contains("/messages/");
        if !is_event {
            continue;
        }
        if !seen.insert(file.clone()) {
            continue;
        }
        let confidence = if lc.contains("asyncapi") {
            1.0
        } else if lc.ends_with(".avsc") {
            0.9
        } else {
            0.75
        };
        out.push(BoundaryCandidate {
            kind: BoundaryKind::Event,
            file: file.clone(),
            confidence,
        });
    }

    out.sort_by(|a, b| {
        boundary_kind_rank(a.kind)
            .cmp(&boundary_kind_rank(b.kind))
            .then_with(|| a.file.cmp(&b.file))
    });
    out
}

fn boundary_kind_rank(kind: BoundaryKind) -> usize {
    match kind {
        BoundaryKind::Http => 0,
        BoundaryKind::Cli => 1,
        BoundaryKind::Event => 2,
        BoundaryKind::Env => 3,
        BoundaryKind::Config => 4,
        BoundaryKind::Db => 5,
    }
}

fn is_entrypoint_candidate(file_lc: &str) -> bool {
    file_lc == "main.rs"
        || file_lc == "main.py"
        || file_lc == "app.py"
        || file_lc == "server.py"
        || file_lc == "index.js"
        || file_lc == "server.js"
        || file_lc == "main.ts"
        || file_lc == "server.ts"
        || file_lc.ends_with("/src/main.rs")
        || file_lc.ends_with("/main.rs")
        || file_lc.ends_with("/main.py")
        || file_lc.ends_with("/app.py")
        || file_lc.ends_with("/server.py")
        || file_lc.ends_with("/index.js")
        || file_lc.ends_with("/server.js")
        || file_lc.ends_with("/main.ts")
        || file_lc.ends_with("/server.ts")
}

fn is_contract_candidate(file_lc: &str) -> bool {
    file_lc.starts_with("contracts/")
        || file_lc.starts_with("proto/")
        || file_lc.contains("/openapi.")
        || file_lc.ends_with(".proto")
        || file_lc.ends_with(".schema.json")
        || file_lc.ends_with("openapi.json")
        || file_lc.ends_with("openapi.yaml")
        || file_lc.ends_with("openapi.yml")
        || file_lc.ends_with("asyncapi.json")
        || file_lc.ends_with("asyncapi.yaml")
        || file_lc.ends_with("asyncapi.yml")
        || file_lc.contains("/asyncapi.")
}

fn contract_kind(file: &str) -> &'static str {
    let lc = file.to_ascii_lowercase();
    if lc.ends_with(".proto") || lc.starts_with("proto/") {
        return "proto";
    }
    if lc.ends_with(".schema.json") {
        return "jsonschema";
    }
    if lc.ends_with("openapi.json") || lc.ends_with("openapi.yaml") || lc.ends_with("openapi.yml") {
        return "openapi";
    }
    if lc.contains("/openapi.") {
        return "openapi";
    }
    if lc.ends_with("asyncapi.json")
        || lc.ends_with("asyncapi.yaml")
        || lc.ends_with("asyncapi.yml")
    {
        return "asyncapi";
    }
    if lc.contains("/asyncapi.") {
        return "asyncapi";
    }
    "contract"
}

async fn extract_asyncapi_flows(root: &Path, contracts: &[String]) -> Vec<FlowEdge> {
    const MAX_READ_BYTES: usize = 256 * 1024;
    const MAX_CHANNELS: usize = 10;

    let mut out: Vec<FlowEdge> = Vec::new();
    for contract in contracts {
        if contract_kind(contract) != "asyncapi" {
            continue;
        }

        let Some(content) = read_file_prefix_utf8(root, contract, MAX_READ_BYTES).await else {
            continue;
        };
        let summary = extract_asyncapi_summary(&content);
        let protocol = summary.protocols.into_iter().next();

        let mut channels = summary.channels;
        channels.sort_by(|a, b| a.name.cmp(&b.name));
        for ch in channels.into_iter().take(MAX_CHANNELS) {
            if ch.publish {
                out.push(FlowEdge {
                    contract_file: contract.clone(),
                    channel: ch.name.clone(),
                    direction: FlowDirection::Publish,
                    protocol: protocol.clone(),
                });
            }
            if ch.subscribe {
                out.push(FlowEdge {
                    contract_file: contract.clone(),
                    channel: ch.name.clone(),
                    direction: FlowDirection::Subscribe,
                    protocol: protocol.clone(),
                });
            }
        }
    }

    out.sort_by(|a, b| {
        a.contract_file
            .cmp(&b.contract_file)
            .then_with(|| a.channel.cmp(&b.channel))
            .then_with(|| a.direction.as_str().cmp(b.direction.as_str()))
    });
    out
}

async fn read_file_prefix_utf8(root: &Path, rel: &str, max_bytes: usize) -> Option<String> {
    let abs = root.join(rel);
    let mut file = File::open(abs).await.ok()?;
    let mut buf = vec![0u8; max_bytes];
    let n = file.read(&mut buf).await.ok()?;
    buf.truncate(n);
    String::from_utf8(buf).ok()
}

fn select_repo_anchors(
    files: &[String],
    entrypoints: &[String],
    contracts: &[String],
    boundaries: &[BoundaryCandidate],
) -> Vec<AnchorCandidate> {
    const MAX_ANCHORS: usize = 7;
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

    out.truncate(MAX_ANCHORS);
    out
}

fn best_canon_doc(files: &[String]) -> Option<String> {
    let mut candidates: Vec<(usize, &String)> = Vec::new();
    for file in files {
        let lc = file.to_ascii_lowercase();
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
            "jsonschema" => 3usize,
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

fn extract_asyncapi_summary(content: &str) -> AsyncApiSummary {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(content) {
        return extract_asyncapi_summary_json(&json);
    }
    extract_asyncapi_summary_yaml_like(content)
}

fn extract_asyncapi_summary_json(value: &serde_json::Value) -> AsyncApiSummary {
    let mut out = AsyncApiSummary::default();

    if let Some(servers) = value.get("servers").and_then(|v| v.as_object()) {
        for server in servers.values() {
            if let Some(protocol) = server.get("protocol").and_then(|v| v.as_str()) {
                let protocol = protocol.trim().to_ascii_lowercase();
                if protocol.is_empty() {
                    continue;
                }
                if !out.protocols.iter().any(|p| p == &protocol) {
                    out.protocols.push(protocol);
                }
            }
        }
    }

    if let Some(channels) = value.get("channels").and_then(|v| v.as_object()) {
        for (name, channel) in channels {
            let publish = channel.get("publish").is_some();
            let subscribe = channel.get("subscribe").is_some();
            out.channels.push(AsyncApiChannel {
                name: name.clone(),
                publish,
                subscribe,
            });
        }
    }

    out
}

fn extract_asyncapi_summary_yaml_like(content: &str) -> AsyncApiSummary {
    let mut out = AsyncApiSummary::default();

    // Best-effort protocol detection: look for `protocol: <value>` lines.
    for raw in content.lines().take(5000) {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("protocol:") else {
            continue;
        };
        let protocol = rest.trim().trim_matches('"').trim_matches('\'');
        if protocol.is_empty() {
            continue;
        }
        let protocol = protocol.to_ascii_lowercase();
        if !out.protocols.iter().any(|p| p == &protocol) {
            out.protocols.push(protocol);
        }
    }

    // Best-effort channel extraction from YAML:
    // channels:
    //   topic.name:
    //     publish:
    //     subscribe:
    let lines: Vec<&str> = content.lines().collect();
    let mut idx = 0usize;
    while idx < lines.len() {
        let raw = lines[idx];
        if raw.trim_start().starts_with("channels:") {
            break;
        }
        idx += 1;
    }
    if idx >= lines.len() {
        return out;
    }

    let channels_indent = count_leading_spaces(lines[idx]);
    idx += 1;

    let mut current: Option<AsyncApiChannel> = None;
    let mut current_indent: usize = 0;

    while idx < lines.len() {
        let raw = lines[idx];
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            idx += 1;
            continue;
        }
        let indent = count_leading_spaces(raw);
        if indent <= channels_indent {
            break;
        }

        if trimmed.ends_with(':') && !trimmed.starts_with('-') {
            let key = trimmed.trim_end_matches(':').trim();
            let key = key.trim_matches('"').trim_matches('\'');
            if !key.is_empty() && key != "publish" && key != "subscribe" {
                if let Some(ch) = current.take() {
                    out.channels.push(ch);
                }
                current_indent = indent;
                current = Some(AsyncApiChannel {
                    name: key.to_string(),
                    publish: false,
                    subscribe: false,
                });
                idx += 1;
                continue;
            }
        }

        if let Some(ch) = current.as_mut() {
            if indent > current_indent {
                if trimmed.starts_with("publish:") {
                    ch.publish = true;
                } else if trimmed.starts_with("subscribe:") {
                    ch.subscribe = true;
                }
            }
        }

        idx += 1;
    }

    if let Some(ch) = current.take() {
        out.channels.push(ch);
    }

    out
}

fn count_leading_spaces(s: &str) -> usize {
    s.as_bytes().iter().take_while(|&&b| b == b' ').count()
}

#[derive(Debug, Clone)]
struct OutlineSymbol {
    kind: &'static str,
    name: String,
    start_line: usize,
    end_line: usize,
    confidence: f32,
}

async fn extract_code_outline(root: &Path, focus_rel: &str) -> Vec<OutlineSymbol> {
    const MAX_FILE_BYTES: u64 = 512 * 1024;
    const MAX_SYMBOLS: usize = 8;

    let focus_lc = focus_rel.to_ascii_lowercase();
    if !is_code_file_candidate(&focus_lc) {
        return Vec::new();
    }

    let abs = root.join(focus_rel);
    let Ok(meta) = tokio::fs::metadata(&abs).await else {
        return Vec::new();
    };
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Vec::new();
    }

    tokio::task::spawn_blocking(move || {
        let chunker = context_code_chunker::Chunker::new(context_code_chunker::ChunkerConfig {
            // Outline is a “meaning read”: avoid pulling long docs into the metadata.
            include_documentation: false,
            ..context_code_chunker::ChunkerConfig::default()
        });
        let chunks = chunker.chunk_file(abs).ok()?;

        let mut seen: HashSet<String> = HashSet::new();
        let mut symbols: Vec<(u8, OutlineSymbol)> = Vec::new();
        for chunk in chunks {
            let Some(chunk_type) = chunk.metadata.chunk_type else {
                continue;
            };
            if !chunk_type.is_declaration() {
                continue;
            }

            let name = chunk.metadata.qualified_name.as_ref().cloned().or_else(|| {
                let symbol = chunk.metadata.symbol_name.as_ref()?.trim();
                if symbol.is_empty() {
                    return None;
                }
                if let Some(scope) = chunk.metadata.parent_scope.as_ref().map(|s| s.trim()) {
                    if !scope.is_empty() {
                        return Some(format!("{scope}.{symbol}"));
                    }
                }
                Some(symbol.to_string())
            });
            let Some(name) = name else {
                continue;
            };

            let key = format!(
                "{}:{}:{}:{}",
                chunk.file_path, chunk.start_line, chunk.end_line, name
            );
            if !seen.insert(key) {
                continue;
            }

            symbols.push((
                chunk_type.priority(),
                OutlineSymbol {
                    kind: chunk_type.as_str(),
                    name,
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    confidence: 1.0,
                },
            ));
        }

        symbols.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.start_line.cmp(&b.1.start_line))
                .then_with(|| a.1.name.cmp(&b.1.name))
        });
        Some(
            symbols
                .into_iter()
                .map(|(_, s)| s)
                .take(MAX_SYMBOLS)
                .collect::<Vec<_>>(),
        )
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
}

async fn detect_channel_mentions(
    root: &Path,
    files: &[String],
    channels: &[String],
) -> HashMap<String, String> {
    const MAX_SCAN_FILES: usize = 200;
    const MAX_READ_BYTES: usize = 64 * 1024;
    const MAX_CHANNELS: usize = 20;

    let mut wanted: Vec<String> = channels.to_vec();
    wanted.sort();
    wanted.dedup();
    wanted.truncate(MAX_CHANNELS);

    let mut out: HashMap<String, String> = HashMap::new();
    if wanted.is_empty() {
        return out;
    }

    let mut candidates: Vec<&String> = files
        .iter()
        .filter(|file| is_code_file_candidate(&file.to_ascii_lowercase()))
        .collect();
    candidates.sort();

    for file in candidates.into_iter().take(MAX_SCAN_FILES) {
        if out.len() >= wanted.len() {
            break;
        }
        let Some(content) = read_file_prefix_utf8(root, file, MAX_READ_BYTES).await else {
            continue;
        };
        for channel in &wanted {
            if out.contains_key(channel) {
                continue;
            }
            if content.contains(channel) {
                out.insert(channel.clone(), file.clone());
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
struct BrokerCandidate {
    proto: String,
    file: String,
    confidence: f32,
}

async fn detect_brokers(root: &Path, files: &[String], flows: &[FlowEdge]) -> Vec<BrokerCandidate> {
    const MAX_CANDIDATE_FILES: usize = 30;
    const MAX_READ_BYTES: usize = 192 * 1024;
    const MAX_BROKERS: usize = 4;

    let mut wanted: Vec<String> = flows
        .iter()
        .filter_map(|f| f.protocol.as_ref())
        .map(|p| p.to_ascii_lowercase())
        .collect();
    wanted.sort();
    wanted.dedup();
    if wanted.is_empty() {
        wanted = vec!["kafka", "nats", "amqp", "mqtt", "pulsar"]
            .into_iter()
            .map(|s| s.to_string())
            .collect();
    }

    let mut candidates: Vec<&String> = files
        .iter()
        .filter(|file| is_broker_config_candidate(&file.to_ascii_lowercase()))
        .collect();
    candidates.sort();

    let mut out: Vec<BrokerCandidate> = Vec::new();
    let mut seen_files: HashSet<&str> = HashSet::new();

    for file in candidates.into_iter().take(MAX_CANDIDATE_FILES) {
        if out.len() >= MAX_BROKERS {
            break;
        }
        if !seen_files.insert(file.as_str()) {
            continue;
        }
        let Some(content) = read_file_prefix_utf8(root, file, MAX_READ_BYTES).await else {
            continue;
        };
        let content_lc = content.to_ascii_lowercase();
        for proto in &wanted {
            if !content_mentions_proto(&content_lc, proto) {
                continue;
            }
            let mut confidence = 0.75;
            if file.to_ascii_lowercase().contains("docker-compose")
                || file.to_ascii_lowercase().ends_with("compose.yml")
                || file.to_ascii_lowercase().ends_with("compose.yaml")
            {
                confidence = 0.9;
            } else if content_lc.contains("image:") {
                confidence = 0.85;
            }
            out.push(BrokerCandidate {
                proto: proto.clone(),
                file: file.clone(),
                confidence,
            });
            break;
        }
    }

    out.sort_by(|a, b| {
        b.confidence
            .total_cmp(&a.confidence)
            .then_with(|| a.proto.cmp(&b.proto))
            .then_with(|| a.file.cmp(&b.file))
    });
    out.truncate(MAX_BROKERS);
    out
}

fn is_code_file_candidate(file_lc: &str) -> bool {
    if file_lc.starts_with("target/")
        || file_lc.contains("/target/")
        || file_lc.starts_with("node_modules/")
        || file_lc.contains("/node_modules/")
        || file_lc.starts_with("vendor/")
        || file_lc.contains("/vendor/")
        || file_lc.starts_with(".git/")
        || file_lc.contains("/.git/")
    {
        return false;
    }
    file_lc.ends_with(".rs")
        || file_lc.ends_with(".go")
        || file_lc.ends_with(".py")
        || file_lc.ends_with(".js")
        || file_lc.ends_with(".ts")
        || file_lc.ends_with(".java")
        || file_lc.ends_with(".kt")
        || file_lc.ends_with(".kts")
        || file_lc.ends_with(".cs")
        || file_lc.ends_with(".cpp")
        || file_lc.ends_with(".c")
        || file_lc.ends_with(".h")
        || file_lc.ends_with(".hpp")
}

fn is_broker_config_candidate(file_lc: &str) -> bool {
    let is_compose = file_lc.ends_with("docker-compose.yml")
        || file_lc.ends_with("docker-compose.yaml")
        || file_lc.ends_with("compose.yml")
        || file_lc.ends_with("compose.yaml");
    if is_compose {
        return true;
    }

    let basename = file_lc.rsplit('/').next().unwrap_or(file_lc);
    if basename == "tiltfile" && file_lc == basename {
        return true;
    }

    let is_tf =
        file_lc.ends_with(".tf") || file_lc.ends_with(".tfvars") || file_lc.ends_with(".hcl");
    if is_tf {
        let is_tf_dir = file_lc.starts_with("terraform/")
            || file_lc.contains("/terraform/")
            || file_lc.starts_with("infra/")
            || file_lc.contains("/infra/");
        let is_tf_root_candidate = matches!(
            basename,
            "main.tf"
                | "variables.tf"
                | "versions.tf"
                | "provider.tf"
                | "providers.tf"
                | "backend.tf"
                | "outputs.tf"
                | "terraform.tf"
                | "terragrunt.hcl"
        );
        let is_root = file_lc == basename;
        return is_tf_dir || (is_root && is_tf_root_candidate) || basename == "terragrunt.hcl";
    }

    let is_infra_dir = file_lc.starts_with("k8s/")
        || file_lc.contains("/k8s/")
        || file_lc.starts_with("kubernetes/")
        || file_lc.contains("/kubernetes/")
        || file_lc.starts_with("manifests/")
        || file_lc.contains("/manifests/")
        || file_lc.starts_with("deploy/")
        || file_lc.contains("/deploy/")
        || file_lc.starts_with("kustomize/")
        || file_lc.contains("/kustomize/")
        || file_lc.starts_with("infra/")
        || file_lc.contains("/infra/")
        || file_lc.starts_with("charts/")
        || file_lc.contains("/charts/")
        || file_lc.contains("/helm/")
        || file_lc.starts_with("argocd/")
        || file_lc.contains("/argocd/")
        || file_lc.starts_with("argo/")
        || file_lc.contains("/argo/")
        || file_lc.starts_with("flux/")
        || file_lc.contains("/flux/")
        || file_lc.starts_with("gitops/")
        || file_lc.contains("/gitops/")
        || file_lc.starts_with("clusters/")
        || file_lc.contains("/clusters/")
        || matches!(
            basename,
            "helmfile.yaml"
                | "helmfile.yml"
                | "helmrelease.yaml"
                | "helmrelease.yml"
                | "kustomization.yaml"
                | "kustomization.yml"
                | "skaffold.yaml"
                | "skaffold.yml"
                | "werf.yaml"
                | "werf.yml"
                | "devspace.yaml"
                | "devspace.yml"
        );
    if !is_infra_dir {
        return false;
    }

    file_lc.ends_with(".yaml") || file_lc.ends_with(".yml")
}

fn content_mentions_proto(content_lc: &str, proto_lc: &str) -> bool {
    match proto_lc {
        "kafka" => {
            content_lc.contains("kafka")
                || content_lc.contains("cp-kafka")
                || content_lc.contains("confluentinc")
                || content_lc.contains("bitnami/kafka")
        }
        "nats" => content_lc.contains("nats") || content_lc.contains("natsio"),
        "amqp" | "rabbitmq" => content_lc.contains("rabbitmq") || content_lc.contains("amqp"),
        "mqtt" => content_lc.contains("mqtt"),
        "pulsar" => content_lc.contains("pulsar"),
        other => content_lc.contains(other),
    }
}

fn infer_actor_by_path(reference_file: &str, entrypoints: &[String]) -> Option<String> {
    let (reference_dir, _) = reference_file.rsplit_once('/')?;
    if reference_dir.is_empty() {
        return None;
    }

    let mut best: Option<&String> = None;
    let mut best_score: usize = 0;
    for ep in entrypoints {
        let ep_dir = ep.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let score = common_prefix_segments(reference_dir, ep_dir);
        if score == 0 {
            continue;
        }

        if score > best_score {
            best = Some(ep);
            best_score = score;
            continue;
        }
        if score == best_score {
            if let Some(current) = best {
                if ep < current {
                    best = Some(ep);
                }
            }
        }
    }
    best.cloned()
}

fn infer_flow_actor(contract_file: &str, entrypoints: &[String]) -> Option<String> {
    if entrypoints.is_empty() {
        return None;
    }

    // Root-level AsyncAPI: safe only when the repo clearly has a single entrypoint.
    let Some(_) = contract_file.rsplit_once('/') else {
        return (entrypoints.len() == 1).then(|| entrypoints[0].clone());
    };
    infer_actor_by_path(contract_file, entrypoints)
}

fn common_prefix_segments(a: &str, b: &str) -> usize {
    a.split('/')
        .filter(|p| !p.is_empty())
        .zip(b.split('/').filter(|p| !p.is_empty()))
        .take_while(|(x, y)| x == y)
        .count()
}

fn json_string(value: &str) -> String {
    // Safe single-line encoding for CP dictionary values.
    serde_json::to_string(value).unwrap_or_else(|_| "\"<invalid>\"".to_string())
}

fn normalize_relative_path(root: &Path, canonical: &Path) -> Option<String> {
    let rel = canonical.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().into_owned().replace('\\', "/"))
}

async fn hash_and_count_lines(path: &Path) -> Result<(String, usize)> {
    let meta = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("Failed to stat {}", path.display()))?;
    let file_size = meta.len();

    let mut file = File::open(path)
        .await
        .with_context(|| format!("Failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut newlines = 0usize;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        newlines += buf[..n].iter().filter(|&&b| b == b'\n').count();
    }
    let hash = format!("{:x}", hasher.finalize());
    let lines = if file_size == 0 { 0 } else { newlines + 1 };
    Ok((hash, lines))
}

async fn read_file_lines_window(
    path: &Path,
    start_line: usize,
    end_line: usize,
    max_lines: usize,
) -> Result<(String, bool)> {
    let file = File::open(path)
        .await
        .with_context(|| format!("Failed to open {}", path.display()))?;
    let mut reader = BufReader::new(file).lines();

    let mut current = 0usize;
    let mut out: Vec<String> = Vec::new();
    let mut truncated = false;
    while let Some(line) = reader.next_line().await? {
        current += 1;
        if current < start_line {
            continue;
        }
        if current > end_line {
            break;
        }
        out.push(line);
        if out.len() >= max_lines {
            truncated = true;
            break;
        }
    }
    Ok((out.join("\n"), truncated))
}

fn shrink_pack(pack: &mut String) -> bool {
    let mut lines: Vec<String> = pack
        .lines()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect();
    if lines.is_empty() {
        return false;
    }

    if lines.first().map(|line| line.as_str()) != Some("CPV1") {
        return shrink_pack_simple(pack);
    }
    let Some(nba_idx) = lines.iter().rposition(|line| line.starts_with("NBA ")) else {
        return shrink_pack_simple(pack);
    };
    if nba_idx == 0 {
        return false;
    }

    let mut changed = remove_one_low_priority_body_line(&mut lines, nba_idx);
    changed |= prune_unused_ev_lines(&mut lines);
    changed |= prune_unused_dict_lines(&mut lines);
    changed |= remove_empty_sections(&mut lines);

    if !changed {
        let mut minimal: Vec<String> = Vec::new();
        if let Some(first) = lines.first() {
            minimal.push(first.clone());
        }
        if let Some(root_fp) = lines.iter().find(|line| line.starts_with("ROOT_FP ")) {
            minimal.push(root_fp.clone());
        }
        if let Some(query) = lines.iter().find(|line| line.starts_with("QUERY ")) {
            minimal.push(query.clone());
        }

        // Prefer preserving at least one evidence pointer for “precision fetch”, even under
        // extreme budgets. This keeps the pack actionable (semantic zoom → exact read).
        if let Some(ev_line) = lines.iter().find(|line| line.starts_with("EV ")) {
            let ev_id = ev_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("ev0")
                .to_string();
            let file_id = ev_line
                .split_whitespace()
                .find_map(|token| token.strip_prefix("file="))
                .map(str::to_string);
            let range = ev_line
                .split_whitespace()
                .find(|token| token.starts_with('L') && token.contains("-L"))
                .unwrap_or("L1-L1")
                .to_string();

            if let Some(file_id) = file_id {
                let dict_prefix = format!("D {file_id} ");
                if let Some(d_line) = lines.iter().find(|line| line.starts_with(&dict_prefix)) {
                    minimal.push("S DICT".to_string());
                    minimal.push(d_line.clone());
                    minimal.push("S EVIDENCE".to_string());
                    minimal.push(ev_line.clone());
                    minimal.push(format!(
                        "NBA evidence_fetch ev={ev_id} file={file_id} {range}"
                    ));
                    *pack = minimal.join("\n") + "\n";
                    return true;
                }
            }
        }

        minimal.push("NBA map".to_string());
        *pack = minimal.join("\n") + "\n";
        return true;
    }

    *pack = lines.join("\n") + "\n";
    true
}

fn shrink_pack_simple(pack: &mut String) -> bool {
    let trimmed = pack.trim_end_matches('\n');
    if trimmed.is_empty() {
        return false;
    }

    let last_line_start = trimmed.rfind('\n').map(|pos| pos + 1).unwrap_or(0);
    let last_line = &trimmed[last_line_start..];
    let is_nba = last_line.starts_with("NBA ");

    if !is_nba {
        if last_line_start < 10 {
            return false;
        }
        pack.truncate(last_line_start);
        return true;
    }

    if last_line_start == 0 {
        return false;
    }
    let before_last = &trimmed[..last_line_start - 1];
    let Some(prev_start) = before_last.rfind('\n').map(|pos| pos + 1) else {
        return false;
    };
    if prev_start < 10 {
        return false;
    }
    let mut rebuilt = String::new();
    rebuilt.push_str(&trimmed[..prev_start]);
    rebuilt.push_str(last_line);
    rebuilt.push('\n');
    *pack = rebuilt;
    true
}

fn remove_one_low_priority_body_line(lines: &mut Vec<String>, nba_idx: usize) -> bool {
    // Keep this deterministic: lowest-signal content is removed first.
    // Note: we intentionally do *not* remove `D ...`, `EV ...`, or headers here.
    const PREFIXES: [&str; 8] = [
        "MAP ",
        "SYM ",
        "BOUNDARY ",
        "FLOW ",
        "BROKER ",
        "ENTRY ",
        "CONTRACT ",
        "ANCHOR ",
    ];

    for prefix in PREFIXES {
        if let Some(idx) = lines
            .iter()
            .take(nba_idx)
            .rposition(|line| line.starts_with(prefix))
        {
            lines.remove(idx);
            return true;
        }
    }

    false
}

fn remove_empty_sections(lines: &mut Vec<String>) -> bool {
    let mut changed = false;
    let mut idx = 0usize;
    while idx < lines.len() {
        if !lines[idx].starts_with("S ") {
            idx += 1;
            continue;
        }

        let start = idx;
        let mut end = start + 1;
        while end < lines.len() && !lines[end].starts_with("S ") && !lines[end].starts_with("NBA ")
        {
            end += 1;
        }
        let has_data = (start + 1) < end;
        if !has_data {
            lines.remove(start);
            changed = true;
            continue;
        }
        idx = end;
    }
    changed
}

fn prune_unused_ev_lines(lines: &mut Vec<String>) -> bool {
    let mut used: HashSet<String> = HashSet::new();
    for line in lines.iter().filter(|line| !line.starts_with("EV ")) {
        for token in line.split_whitespace() {
            if let Some(ev) = token.strip_prefix("ev=") {
                used.insert(ev.to_string());
            }
        }
    }

    let mut changed = false;
    let mut idx = 0usize;
    while idx < lines.len() {
        if !lines[idx].starts_with("EV ") {
            idx += 1;
            continue;
        }
        let keep = lines[idx]
            .split_whitespace()
            .nth(1)
            .map(|id| used.contains(id))
            .unwrap_or(false);
        if !keep {
            lines.remove(idx);
            changed = true;
            continue;
        }
        idx += 1;
    }

    changed
}

fn prune_unused_dict_lines(lines: &mut Vec<String>) -> bool {
    let mut used: HashSet<String> = HashSet::new();
    for line in lines.iter().filter(|line| !line.starts_with("D ")) {
        collect_dict_ids(line, &mut used);
    }

    let mut changed = false;
    let mut idx = 0usize;
    while idx < lines.len() {
        if !lines[idx].starts_with("D ") {
            idx += 1;
            continue;
        }
        let keep = lines[idx]
            .split_whitespace()
            .nth(1)
            .map(|id| used.contains(id))
            .unwrap_or(false);
        if !keep {
            lines.remove(idx);
            changed = true;
            continue;
        }
        idx += 1;
    }
    changed
}

fn collect_dict_ids(line: &str, out: &mut HashSet<String>) {
    let bytes = line.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] != b'd' || idx + 1 >= bytes.len() || !bytes[idx + 1].is_ascii_digit() {
            idx += 1;
            continue;
        }
        let start = idx;
        let mut end = idx + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        out.insert(line[start..end].to_string());
        idx = end;
    }
}

struct CognitivePack {
    dict: Vec<String>,
    dict_index: BTreeMap<String, usize>,
    lines: Vec<String>,
}

impl CognitivePack {
    fn new() -> Self {
        Self {
            dict: Vec::new(),
            dict_index: BTreeMap::new(),
            lines: Vec::new(),
        }
    }

    fn dict_intern(&mut self, value: String) {
        if self.dict_index.contains_key(&value) {
            return;
        }
        let idx = self.dict.len();
        self.dict.push(value.clone());
        self.dict_index.insert(value, idx);
    }

    fn dict_id(&self, value: &str) -> String {
        let idx = *self
            .dict_index
            .get(value)
            .unwrap_or_else(|| panic!("missing dict entry for {value}"));
        format!("d{idx}")
    }

    fn push_line(&mut self, line: &str) {
        self.lines.push(line.to_string());
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(line);
            out.push('\n');
        }
        if !self.dict.is_empty() {
            let mut dict_block = String::new();
            dict_block.push_str("S DICT\n");
            for (idx, value) in self.dict.iter().enumerate() {
                dict_block.push_str(&format!("D d{idx} {}\n", json_string(value)));
            }
            // Place dictionary immediately after header section.
            // The CP is small; we keep a stable insertion point at line 3.
            let lines = out.lines().collect::<Vec<_>>();
            let insert_at = lines.len().min(3);
            let mut rebuilt = String::new();
            for (i, line) in lines.iter().enumerate() {
                if i == insert_at {
                    rebuilt.push_str(&dict_block);
                }
                rebuilt.push_str(line);
                rebuilt.push('\n');
            }
            if insert_at == lines.len() {
                rebuilt.push_str(&dict_block);
            }
            return rebuilt;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_ev_ref(line: &str) -> Option<&str> {
        line.split_whitespace()
            .find_map(|token| token.strip_prefix("ev="))
    }

    fn assert_cp_claims_have_evidence(pack: &str) {
        let mut ev_ids: HashSet<&str> = HashSet::new();
        for line in pack.lines() {
            if line.starts_with("EV ") {
                if let Some(id) = line
                    .strip_prefix("EV ")
                    .and_then(|rest| rest.split_whitespace().next())
                {
                    ev_ids.insert(id);
                }
            }
        }
        assert!(!ev_ids.is_empty(), "expected at least one EV line");

        for line in pack.lines() {
            let is_claim = line.starts_with("ENTRY ")
                || line.starts_with("CONTRACT ")
                || line.starts_with("BOUNDARY ")
                || line.starts_with("FLOW ")
                || line.starts_with("BROKER ")
                || line.starts_with("ANCHOR ");
            if !is_claim {
                continue;
            }
            let Some(ev) = extract_ev_ref(line) else {
                panic!("claim missing ev= pointer: {line}");
            };
            assert!(
                ev_ids.contains(ev),
                "claim references missing EV ({ev}): {line}"
            );
        }

        let nba = pack
            .lines()
            .find(|line| line.starts_with("NBA "))
            .expect("expected NBA line");
        if nba.contains("evidence_fetch") {
            let Some(ev) = extract_ev_ref(nba) else {
                panic!("NBA missing ev= pointer: {nba}");
            };
            assert!(
                ev_ids.contains(ev),
                "NBA references missing EV ({ev}): {nba}"
            );
        }
    }

    #[test]
    fn shrink_pack_preserves_claim_evidence_invariants() {
        let mut pack = [
            "CPV1",
            "ROOT_FP 1",
            "QUERY \"q\"",
            "S DICT",
            "D d0 \"src/main.rs\"",
            "D d1 \"contracts/a.schema.json\"",
            "D d2 \"src/extra.rs\"",
            "S MAP",
            "MAP path=d2 files=999",
            "MAP path=d2 files=999",
            "MAP path=d2 files=999",
            "S ENTRYPOINTS",
            "ENTRY file=d0 ev=ev0",
            "S CONTRACTS",
            "CONTRACT kind=jsonschema file=d1 ev=ev1",
            "S EVIDENCE",
            "EV ev0 kind=entrypoint file=d0 L1-L2 sha256=aa",
            "EV ev1 kind=contract file=d1 L1-L2 sha256=bb",
            "NBA evidence_fetch ev=ev0 file=d0 L1-L2",
        ]
        .join("\n")
            + "\n";

        for _ in 0..64 {
            assert_cp_claims_have_evidence(&pack);
            let before = pack.chars().count();
            if !shrink_pack(&mut pack) {
                break;
            }
            let after = pack.chars().count();
            assert!(after <= before, "expected shrink to not increase size");
        }

        assert!(pack.contains("\nNBA "), "expected NBA line to remain");
        assert_cp_claims_have_evidence(&pack);
    }
}
