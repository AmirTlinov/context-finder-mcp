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
        const MAX_BOUNDARIES: usize = 12;
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

        let mut map_rows = dir_chunks
            .iter()
            .map(|(path, chunks)| {
                let files = dir_files.get(path).map(|set| set.len()).unwrap_or(0);
                (path.clone(), files, *chunks)
            })
            .collect::<Vec<_>>();
        map_rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        map_rows.truncate(MAP_LIMIT);

        let (entrypoints, contracts) = classify_files(&all_files);
        let mut boundaries = classify_boundaries(&all_files, &entrypoints, &contracts);
        boundaries.truncate(MAX_BOUNDARIES);

        let flows = extract_asyncapi_flows(&project_ctx.root, &contracts).await;
        let files_vec = all_files.iter().cloned().collect::<Vec<_>>();
        let channels = flows.iter().map(|f| f.channel.clone()).collect::<Vec<_>>();
        let channel_mentions =
            detect_channel_mentions(&project_ctx.root, &files_vec, &channels).await;
        let brokers = detect_brokers(&project_ctx.root, &files_vec, &flows).await;

        let mut evidence_candidates: Vec<(EvidenceKind, String)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

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

        let mut cp = CognitivePack::new();
        cp.push_line("CPV1");
        cp.push_line(&format!("ROOT_FP {root_fp}"));
        cp.push_line(&format!("QUERY {}", json_string(&payload.query)));

        let mut dict_paths = BTreeSet::new();
        for (path, _, _) in &map_rows {
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
        for (path, files, chunks) in &map_rows {
            let d = cp.dict_id(path);
            cp.push_line(&format!("MAP path={d} files={files} chunks={chunks}"));
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
                let kind = contract_kind(file);
                let ev = ev_file_index
                    .get(file)
                    .map(|id| format!(" ev={id}"))
                    .unwrap_or_default();
                cp.push_line(&format!("CONTRACT kind={kind} file={d}{ev}"));
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
                let kind = match ev.kind {
                    EvidenceKind::Entrypoint => "entrypoint".to_string(),
                    EvidenceKind::Contract => "contract".to_string(),
                    EvidenceKind::Boundary(kind) => format!("boundary.{}", kind.as_str()),
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
            .first()
            .map(|ev| {
                let ev_id = ev_file_index
                    .get(ev.file.as_str())
                    .map(|id| format!(" ev={id}"))
                    .unwrap_or_default();
                let d = cp.dict_id(&ev.file);
                format!(
                    "NBA evidence_fetch{ev_id} file={d} L{}-L{}",
                    ev.start_line, ev.end_line
                )
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

        let mut cp = CognitivePack::new();
        cp.push_line("CPV1");
        cp.push_line(&format!("ROOT_FP {root_fp}"));
        cp.push_line(&format!("QUERY {}", json_string(&query)));

        let mut dict_paths = BTreeSet::new();
        dict_paths.insert(focus_dir.clone());
        dict_paths.insert(focus_rel.clone());
        for (path, _, _) in &map_rows {
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
        for (path, files, chunks) in &map_rows {
            let d = cp.dict_id(path);
            cp.push_line(&format!("MAP path={d} files={files} chunks={chunks}"));
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
                let kind = contract_kind(file);
                let ev = ev_file_index
                    .get(file)
                    .map(|id| format!(" ev={id}"))
                    .unwrap_or_default();
                cp.push_line(&format!("CONTRACT kind={kind} file={d}{ev}"));
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
                let kind = match ev.kind {
                    EvidenceKind::Entrypoint => "entrypoint".to_string(),
                    EvidenceKind::Contract => "contract".to_string(),
                    EvidenceKind::Boundary(kind) => format!("boundary.{}", kind.as_str()),
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
            .first()
            .map(|ev| {
                let ev_id = ev_file_index
                    .get(ev.file.as_str())
                    .map(|id| format!(" ev={id}"))
                    .unwrap_or_default();
                let d = cp.dict_id(&ev.file);
                format!(
                    "NBA evidence_fetch{ev_id} file={d} L{}-L{}",
                    ev.start_line, ev.end_line
                )
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
    file_lc.ends_with("/src/main.rs")
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

    let is_k8s_dir = file_lc.starts_with("k8s/")
        || file_lc.contains("/k8s/")
        || file_lc.starts_with("kubernetes/")
        || file_lc.contains("/kubernetes/")
        || file_lc.starts_with("manifests/")
        || file_lc.contains("/manifests/")
        || file_lc.starts_with("deploy/")
        || file_lc.contains("/deploy/")
        || file_lc.starts_with("infra/")
        || file_lc.contains("/infra/")
        || file_lc.starts_with("charts/")
        || file_lc.contains("/charts/")
        || file_lc.contains("/helm/");
    if !is_k8s_dir {
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
    // Deterministic shrink while preserving the last `NBA ...` line when present.
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
