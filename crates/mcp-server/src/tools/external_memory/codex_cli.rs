use crate::tools::schemas::read_pack::{ReadPackExternalMemoryHit, ReadPackExternalMemoryResult};
use crate::tools::schemas::response_mode::ResponseMode;
use context_vector_store::{CONTEXT_DIR_NAME, LEGACY_CONTEXT_DIR_NAME};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct StoredCandidate {
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ts_ms: Option<u64>,
    embed_text: String,
    excerpt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reference: Option<Value>,
    content_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    semantic_key: Option<String>,
    session_id: String,
    source_rel: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct CodexSessionsCacheV1 {
    v: u32,
    built_at_unix_ms: u64,
    sessions_root: String,
    processed_session_mtime_ms: HashMap<String, u64>,
    #[serde(default)]
    processed_session_progress: HashMap<String, CodexSessionProgress>,
    candidates: Vec<StoredCandidate>,
}

const CODEX_SESSIONS_CACHE_VERSION: u32 = 3;

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct CodexSessionProgress {
    /// Byte cursor into the session `.jsonl` file, always advanced to the last processed `\n`.
    #[serde(default)]
    cursor_bytes: u64,
    #[serde(default)]
    last_seen_len_bytes: u64,
    #[serde(default)]
    last_seen_mtime_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inode: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dev: Option<u64>,
}

pub(super) async fn overlay_for_query(
    root: &Path,
    query: &str,
    response_mode: ResponseMode,
) -> Option<ReadPackExternalMemoryResult> {
    let query = query.trim();
    if query.is_empty() || response_mode == ResponseMode::Minimal {
        return None;
    }

    let mut candidates = load_candidates(root, response_mode, RefreshKind::Query).await?;
    if candidates.is_empty() {
        return None;
    }

    super::apply_lexical_scores(&mut candidates, query);
    candidates.sort_by(|a, b| {
        super::kind_priority(&b.kind)
            .cmp(&super::kind_priority(&a.kind))
            .then_with(|| b.lexical_score.cmp(&a.lexical_score))
            .then_with(|| b.ts_ms.unwrap_or(0).cmp(&a.ts_ms.unwrap_or(0)))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    candidates.truncate(super::MAX_CANDIDATES);

    let selected = super::select_for_embedding(&candidates);
    let hits = super::rank_candidates(query, selected, response_mode).await;
    if hits.is_empty() {
        return None;
    }

    Some(ReadPackExternalMemoryResult {
        source: "codex_cli".to_string(),
        path: None,
        hits,
    })
}

pub(super) async fn overlay_recent(
    root: &Path,
    response_mode: ResponseMode,
) -> Option<ReadPackExternalMemoryResult> {
    if response_mode == ResponseMode::Minimal {
        return None;
    }

    let mut candidates = load_candidates(root, response_mode, RefreshKind::Recent).await?;
    if candidates.is_empty() {
        return None;
    }

    if response_mode == ResponseMode::Facts {
        // Facts mode is the default "daily driver". Keep it low-noise: conversational and
        // raw command traces should not crowd out engineering conclusions.
        candidates.retain(|c| {
            matches!(
                c.kind.trim().to_ascii_lowercase().as_str(),
                "decision"
                    | "plan"
                    | "blocker"
                    | "evidence"
                    | "change"
                    | "requirement"
                    | "requirements"
                    | "note"
                    | "trace"
                    | "tool_output"
            )
        });
    }

    candidates.sort_by(|a, b| {
        super::kind_priority(&b.kind)
            .cmp(&super::kind_priority(&a.kind))
            .then_with(|| b.ts_ms.unwrap_or(0).cmp(&a.ts_ms.unwrap_or(0)))
            .then_with(|| a.kind.cmp(&b.kind))
    });

    let caps = super::diversity_caps(response_mode);
    let mut diversity = super::DiversityState::default();
    let mut hits: Vec<ReadPackExternalMemoryHit> = Vec::new();
    for candidate in candidates {
        if hits.len() >= super::DEFAULT_MAX_HITS {
            break;
        }
        if !super::allow_candidate_kind(&candidate.kind, &mut diversity, caps) {
            continue;
        }
        let score = 1.0 - (hits.len() as f32 * 0.01);
        hits.push(ReadPackExternalMemoryHit {
            kind: candidate.kind,
            title: candidate.title,
            score,
            ts_ms: candidate.ts_ms,
            excerpt: candidate.excerpt,
            reference: candidate.reference,
        });
    }

    if hits.is_empty() {
        return None;
    }

    Some(ReadPackExternalMemoryResult {
        source: "codex_cli".to_string(),
        path: None,
        hits,
    })
}

#[derive(Clone, Copy, Debug)]
enum RefreshKind {
    Query,
    Recent,
}

async fn load_candidates(
    project_root: &Path,
    response_mode: ResponseMode,
    refresh_kind: RefreshKind,
) -> Option<Vec<super::Candidate>> {
    let codex_home = codex_home_dir_for_project(project_root)?;
    let sessions_root = codex_sessions_root(&codex_home)?;
    let sessions_root_str = sessions_root.to_string_lossy().to_string();

    let cache_path = cache_path_for_project(&codex_home, project_root).await?;
    let now_ms = now_unix_ms();

    let mut cache = load_cache(&cache_path)
        .await
        .unwrap_or_else(|| CodexSessionsCacheV1 {
            v: CODEX_SESSIONS_CACHE_VERSION,
            built_at_unix_ms: 0,
            sessions_root: sessions_root_str.clone(),
            processed_session_mtime_ms: HashMap::new(),
            processed_session_progress: HashMap::new(),
            candidates: Vec::new(),
        });

    if cache.v != CODEX_SESSIONS_CACHE_VERSION {
        cache = CodexSessionsCacheV1 {
            v: CODEX_SESSIONS_CACHE_VERSION,
            built_at_unix_ms: 0,
            sessions_root: sessions_root_str.clone(),
            processed_session_mtime_ms: HashMap::new(),
            processed_session_progress: HashMap::new(),
            candidates: Vec::new(),
        };
    }

    if cache.sessions_root != sessions_root_str {
        cache = CodexSessionsCacheV1 {
            v: CODEX_SESSIONS_CACHE_VERSION,
            built_at_unix_ms: 0,
            sessions_root: sessions_root_str.clone(),
            processed_session_mtime_ms: HashMap::new(),
            processed_session_progress: HashMap::new(),
            candidates: Vec::new(),
        };
    }

    let refresh_interval_ms = match (refresh_kind, response_mode) {
        (RefreshKind::Query, ResponseMode::Full) => 3_000,
        (RefreshKind::Query, ResponseMode::Facts) => 7_000,
        (RefreshKind::Query, ResponseMode::Minimal) => 30_000,
        (RefreshKind::Recent, ResponseMode::Full) => 7_000,
        (RefreshKind::Recent, ResponseMode::Facts) => 20_000,
        (RefreshKind::Recent, ResponseMode::Minimal) => 60_000,
    };

    let cache_age_ms = now_ms.saturating_sub(cache.built_at_unix_ms);
    if cache_age_ms >= refresh_interval_ms {
        cache = refresh_cache(project_root, &sessions_root, cache, response_mode).await;
        let _ = write_cache(&cache_path, &cache).await;
    }

    let mut candidates: Vec<super::Candidate> = cache
        .candidates
        .iter()
        .cloned()
        .map(|stored| super::Candidate {
            kind: stored.kind,
            title: stored.title,
            ts_ms: stored.ts_ms,
            embed_text: stored.embed_text,
            excerpt: stored.excerpt,
            reference: stored.reference,
            lexical_score: 0,
        })
        .collect();
    candidates.truncate(super::MAX_CANDIDATES);
    Some(candidates)
}

async fn refresh_cache(
    project_root: &Path,
    sessions_root: &Path,
    mut cache: CodexSessionsCacheV1,
    response_mode: ResponseMode,
) -> CodexSessionsCacheV1 {
    const MAX_SESSION_FILES_SCAN: usize = 96;
    const MAX_SESSIONS_PROCESS: usize = 24;
    const MAX_TOTAL_CANDIDATES: usize = 240;

    let recent = list_recent_rollout_jsonl(sessions_root, MAX_SESSION_FILES_SCAN).await;
    if recent.is_empty() {
        cache.built_at_unix_ms = now_unix_ms();
        return cache;
    }

    // Canonicalize once for stable prefix matching (best-effort).
    let canonical_root = tokio::fs::canonicalize(project_root).await.ok();
    if canonical_root.is_none() && !project_root.exists() {
        cache.built_at_unix_ms = now_unix_ms();
        return cache;
    }

    // Refresh semantic keys in-place so cache upgrades (and algorithm tweaks) are effective without
    // forcing users to delete caches manually.
    for cand in &mut cache.candidates {
        cand.semantic_key = semantic_key_for_candidate(&cand.kind, &cand.embed_text);
    }

    let mut new_candidates: Vec<StoredCandidate> = Vec::new();
    let mut seen_hashes: HashSet<String> = cache
        .candidates
        .iter()
        .map(|c| c.content_sha256.clone())
        .collect();
    let seen_semantic_keys: HashMap<String, usize> = cache
        .candidates
        .iter()
        .enumerate()
        .filter_map(|(idx, cand)| cand.semantic_key.as_ref().map(|k| (k.clone(), idx)))
        .collect();
    let mut new_semantic_keys: HashMap<String, usize> = HashMap::new();
    let mut processed = 0usize;

    for path in recent {
        if processed >= MAX_SESSIONS_PROCESS {
            break;
        }

        let Some(meta) = probe_session_meta(sessions_root, &path).await else {
            continue;
        };
        let mut matches_root = meta.cwd.starts_with(project_root);
        if !matches_root {
            if let Some(canonical_root) = canonical_root.as_ref() {
                if let Ok(canonical_cwd) = tokio::fs::canonicalize(&meta.cwd).await {
                    matches_root = canonical_cwd.starts_with(canonical_root);
                }
            }
        }
        if !matches_root {
            continue;
        }

        let previous_mtime = cache
            .processed_session_mtime_ms
            .get(&meta.session_id)
            .copied()
            .unwrap_or(0);

        let progress = cache
            .processed_session_progress
            .entry(meta.session_id.clone())
            .or_default();

        // Cache upgrade path (v3 -> v3+cursor): if we previously processed this session (based on
        // mtime) but do not have a cursor yet, assume we are already caught up so we do not
        // re-scan the entire file.
        if progress.cursor_bytes == 0
            && previous_mtime > 0
            && meta.mtime_ms <= previous_mtime
            && meta.len_bytes > 0
        {
            progress.cursor_bytes = meta.len_bytes;
            progress.last_seen_len_bytes = meta.len_bytes;
            progress.last_seen_mtime_ms = meta.mtime_ms;
            progress.inode = meta.inode;
            progress.dev = meta.dev;
            continue;
        }

        if !progress_matches_session_file(progress, &meta) || meta.len_bytes < progress.cursor_bytes
        {
            // File was replaced/truncated: reset cursor so we re-align and re-ingest.
            progress.cursor_bytes = 0;
        }

        let needs_more_bytes = meta.len_bytes > progress.cursor_bytes;
        let needs_mtime = meta.mtime_ms > previous_mtime;
        if !needs_more_bytes && !needs_mtime {
            continue;
        }

        let start_offset = if progress.cursor_bytes == 0 {
            seed_session_cursor(meta.len_bytes, response_mode)
        } else {
            progress.cursor_bytes
        };

        let Some((lines, new_cursor)) =
            read_jsonl_lines_since(&path, start_offset, response_mode).await
        else {
            progress.last_seen_len_bytes = meta.len_bytes;
            progress.last_seen_mtime_ms = meta.mtime_ms;
            progress.inode = meta.inode;
            progress.dev = meta.dev;
            cache
                .processed_session_mtime_ms
                .insert(meta.session_id.clone(), meta.mtime_ms);
            processed = processed.saturating_add(1);
            continue;
        };

        let extracted =
            extract_candidates_from_jsonl_lines(project_root, lines, &meta, response_mode);
        for cand in extracted {
            if !seen_hashes.insert(cand.content_sha256.clone()) {
                continue;
            }

            if let Some(key) = cand.semantic_key.clone() {
                if let Some(existing_idx) = seen_semantic_keys.get(&key).copied() {
                    merge_stored_candidate(
                        &mut cache.candidates[existing_idx],
                        cand,
                        response_mode,
                    );
                    seen_hashes.insert(cache.candidates[existing_idx].content_sha256.clone());
                    continue;
                }
                if let Some(new_idx) = new_semantic_keys.get(&key).copied() {
                    merge_stored_candidate(&mut new_candidates[new_idx], cand, response_mode);
                    seen_hashes.insert(new_candidates[new_idx].content_sha256.clone());
                    continue;
                }
                new_semantic_keys.insert(key, new_candidates.len());
            }

            new_candidates.push(cand);
        }

        progress.cursor_bytes = new_cursor.max(progress.cursor_bytes);
        progress.last_seen_len_bytes = meta.len_bytes;
        progress.last_seen_mtime_ms = meta.mtime_ms;
        progress.inode = meta.inode;
        progress.dev = meta.dev;

        cache
            .processed_session_mtime_ms
            .insert(meta.session_id.clone(), meta.mtime_ms);
        processed = processed.saturating_add(1);
    }

    if !new_candidates.is_empty() {
        cache.candidates.extend(new_candidates);
        cache
            .candidates
            .sort_by(|a, b| b.ts_ms.unwrap_or(0).cmp(&a.ts_ms.unwrap_or(0)));
        cache.candidates.truncate(MAX_TOTAL_CANDIDATES);
    }

    cache.built_at_unix_ms = now_unix_ms();
    cache
}

#[derive(Clone, Debug)]
struct CodexSessionMeta {
    session_id: String,
    cwd: PathBuf,
    mtime_ms: u64,
    len_bytes: u64,
    inode: Option<u64>,
    dev: Option<u64>,
    source_rel: String,
}

async fn probe_session_meta(sessions_root: &Path, path: &Path) -> Option<CodexSessionMeta> {
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut line = String::new();
    for _ in 0..8 {
        line.clear();
        if reader.read_line(&mut line).await.ok()? == 0 {
            break;
        }
        let raw = line.trim();
        if raw.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let payload = value.get("payload")?;
        let session_id = payload.get("id")?.as_str()?.to_string();
        let cwd = PathBuf::from(payload.get("cwd")?.as_str()?);

        let meta = tokio::fs::metadata(path).await.ok()?;
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|ts| ts.duration_since(UNIX_EPOCH).ok())
            .map(|dur| dur.as_millis() as u64)
            .unwrap_or_else(now_unix_ms);
        let len_bytes = meta.len();

        #[cfg(unix)]
        let (inode, dev) = {
            use std::os::unix::fs::MetadataExt;
            (Some(meta.ino()), Some(meta.dev()))
        };

        #[cfg(not(unix))]
        let (inode, dev) = (None, None);

        let source_rel = path
            .strip_prefix(sessions_root)
            .ok()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());

        return Some(CodexSessionMeta {
            session_id,
            cwd,
            mtime_ms,
            len_bytes,
            inode,
            dev,
            source_rel,
        });
    }
    None
}

fn progress_matches_session_file(progress: &CodexSessionProgress, meta: &CodexSessionMeta) -> bool {
    if let (Some(a), Some(b)) = (progress.inode, meta.inode) {
        if a != b {
            return false;
        }
    }
    if let (Some(a), Some(b)) = (progress.dev, meta.dev) {
        if a != b {
            return false;
        }
    }
    true
}

fn seed_session_cursor(file_len_bytes: u64, response_mode: ResponseMode) -> u64 {
    let seed_bytes: u64 = match response_mode {
        ResponseMode::Minimal => 96 * 1024,
        ResponseMode::Facts => 192 * 1024,
        ResponseMode::Full => 384 * 1024,
    };
    file_len_bytes.saturating_sub(seed_bytes)
}

#[derive(Clone, Copy, Debug)]
struct JsonlReadLimits {
    max_read_bytes: usize,
    max_lines: usize,
    max_line_bytes: usize,
}

fn jsonl_read_limits(response_mode: ResponseMode) -> JsonlReadLimits {
    // For incremental ingestion, prefer bounded work per refresh; the byte cursor guarantees we
    // don't miss events even for very long sessions (we just catch up over multiple refreshes).
    match response_mode {
        ResponseMode::Minimal => JsonlReadLimits {
            max_read_bytes: 192 * 1024,
            max_lines: 2_000,
            max_line_bytes: 512 * 1024,
        },
        ResponseMode::Facts => JsonlReadLimits {
            max_read_bytes: 768 * 1024,
            max_lines: 4_000,
            max_line_bytes: 1024 * 1024,
        },
        ResponseMode::Full => JsonlReadLimits {
            max_read_bytes: 2 * 1024 * 1024,
            max_lines: 6_000,
            max_line_bytes: 2 * 1024 * 1024,
        },
    }
}

async fn read_jsonl_lines_since(
    path: &Path,
    start_offset: u64,
    response_mode: ResponseMode,
) -> Option<(Vec<String>, u64)> {
    let limits = jsonl_read_limits(response_mode);

    let mut file = tokio::fs::File::open(path).await.ok()?;
    let meta = file.metadata().await.ok()?;
    let len = meta.len();
    let start = start_offset.min(len);
    let mut aligned = start == 0;
    if !aligned && start > 0 {
        let mut prev = [0u8; 1];
        if file
            .seek(std::io::SeekFrom::Start(start.saturating_sub(1)))
            .await
            .is_ok()
            && file.read_exact(&mut prev).await.is_ok()
        {
            aligned = prev[0] == b'\n';
        }
    }
    file.seek(std::io::SeekFrom::Start(start)).await.ok()?;

    let mut reader = tokio::io::BufReader::new(file);

    let mut tmp = [0u8; 4096];
    let mut cursor = start;
    let mut last_boundary = start;

    let mut line_buf: Vec<u8> = Vec::new();
    let mut oversized = false;
    let mut bytes_used: usize = 0;
    let mut lines: Vec<String> = Vec::new();
    let mut stop_after_boundary = false;

    loop {
        if stop_after_boundary {
            break;
        }
        if aligned && line_buf.is_empty() && bytes_used >= limits.max_read_bytes {
            break;
        }
        if lines.len() >= limits.max_lines {
            break;
        }

        let n = reader.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }

        for byte in tmp[..n].iter().copied() {
            bytes_used = bytes_used.saturating_add(1);
            cursor = cursor.saturating_add(1);

            if byte == b'\n' {
                last_boundary = cursor;

                if !aligned {
                    aligned = true;
                    line_buf.clear();
                    oversized = false;
                    continue;
                }

                if !oversized {
                    if line_buf.last() == Some(&b'\r') {
                        line_buf.pop();
                    }
                    if let Ok(text) = std::str::from_utf8(&line_buf) {
                        let text = text.trim();
                        if !text.is_empty() {
                            lines.push(text.to_string());
                        }
                    }
                }

                line_buf.clear();
                oversized = false;

                if bytes_used >= limits.max_read_bytes {
                    stop_after_boundary = true;
                    break;
                }
                if lines.len() >= limits.max_lines {
                    stop_after_boundary = true;
                    break;
                }
                continue;
            }

            if !oversized {
                if line_buf.len() < limits.max_line_bytes {
                    line_buf.push(byte);
                } else {
                    oversized = true;
                }
            }
        }
    }

    // JSONL files are usually newline-terminated. If the writer omitted the final newline, only
    // treat the trailing line as complete if it parses as JSON; otherwise keep the cursor at the
    // last newline boundary so we don't lose a partially-written event.
    if cursor > last_boundary
        && aligned
        && !oversized
        && !line_buf.is_empty()
        && lines.len() < limits.max_lines
    {
        if line_buf.last() == Some(&b'\r') {
            line_buf.pop();
        }
        if let Ok(text) = std::str::from_utf8(&line_buf) {
            let text = text.trim();
            if !text.is_empty() && serde_json::from_str::<Value>(text).is_ok() {
                lines.push(text.to_string());
                // Advance the boundary to the current cursor (EOF).
                last_boundary = cursor;
            }
        }
    }

    Some((lines, last_boundary))
}

fn extract_candidates_from_jsonl_lines(
    project_root: &Path,
    lines: impl IntoIterator<Item = String>,
    meta: &CodexSessionMeta,
    response_mode: ResponseMode,
) -> Vec<StoredCandidate> {
    let mut out: Vec<StoredCandidate> = Vec::new();
    let mut prompt_count = 0usize;
    let mut requirement_count = 0usize;
    let mut reply_count = 0usize;
    let mut decision_count = 0usize;
    let mut blocker_count = 0usize;
    let mut change_section_count = 0usize;
    let mut patch_count = 0usize;
    let mut plan_count = 0usize;
    let mut evidence_section_count = 0usize;
    let mut command_count = 0usize;
    let mut tool_output_count = 0usize;

    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let Some(payload_type) = payload.get("type").and_then(Value::as_str) else {
            continue;
        };
        let ts_ms = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_utc_ms)
            .or(Some(meta.mtime_ms));

        match payload_type {
            "message" => {
                let Some(role) = payload.get("role").and_then(Value::as_str) else {
                    continue;
                };
                if role != "user" && role != "assistant" {
                    continue;
                }
                let text = extract_message_text(payload);
                if is_noise_transcript_message(role, &text) {
                    continue;
                }
                if role == "assistant" {
                    let sections = extract_high_signal_sections_from_assistant_reply(&text);
                    if !sections.is_empty() {
                        for section in sections {
                            match section.kind {
                                "decision" => {
                                    if decision_count >= 2 {
                                        continue;
                                    }
                                    decision_count = decision_count.saturating_add(1);
                                }
                                "change" => {
                                    if change_section_count >= 2 {
                                        continue;
                                    }
                                    change_section_count = change_section_count.saturating_add(1);
                                }
                                "blocker" => {
                                    if blocker_count >= 2 {
                                        continue;
                                    }
                                    blocker_count = blocker_count.saturating_add(1);
                                }
                                "plan" => {
                                    if plan_count >= 2 {
                                        continue;
                                    }
                                    plan_count = plan_count.saturating_add(1);
                                }
                                "evidence" => {
                                    if evidence_section_count >= 2 {
                                        continue;
                                    }
                                    evidence_section_count =
                                        evidence_section_count.saturating_add(1);
                                }
                                _ => {}
                            }

                            let title = Some(section.title.to_string());
                            let embed_text = super::build_embed_text(
                                section.kind,
                                title.as_deref(),
                                &section.body,
                                1_024,
                            );
                            let excerpt = super::trim_to_chars(
                                &embed_text,
                                super::excerpt_chars(response_mode),
                            );
                            out.push(stored_candidate(
                                section.kind,
                                title,
                                ts_ms.unwrap_or(meta.mtime_ms),
                                embed_text,
                                excerpt,
                                Some(serde_json::json!({
                                    "session_id": meta.session_id,
                                    "source": meta.source_rel,
                                    "role": role,
                                    "section": section.title,
                                })),
                                meta,
                            ));
                        }
                        // Prefer extracted high-signal sections over full conversational replies.
                        continue;
                    }
                }

                if role == "user" && requirement_count < 2 {
                    if let Some(cand) = candidate_from_user_requirements(
                        &text,
                        meta,
                        response_mode,
                        ts_ms.unwrap_or(meta.mtime_ms),
                    ) {
                        requirement_count = requirement_count.saturating_add(1);
                        out.push(cand);
                    }
                }

                if role == "user" {
                    if prompt_count >= 4 {
                        continue;
                    }
                    prompt_count = prompt_count.saturating_add(1);
                } else if reply_count >= 3 {
                    continue;
                } else {
                    reply_count = reply_count.saturating_add(1);
                }

                let kind = if role == "user" { "prompt" } else { "reply" };
                let title = first_line_title(&text, 80);
                let embed_text = super::build_embed_text(kind, title.as_deref(), &text, 2_048);
                let excerpt =
                    super::trim_to_chars(&embed_text, super::excerpt_chars(response_mode));
                out.push(stored_candidate(
                    kind,
                    title,
                    ts_ms.unwrap_or(meta.mtime_ms),
                    embed_text,
                    excerpt,
                    Some(serde_json::json!({
                        "session_id": meta.session_id,
                        "source": meta.source_rel,
                        "role": role,
                    })),
                    meta,
                ));
            }
            "function_call" => {
                let Some(name) = payload.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let args = payload
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if name == "update_plan" {
                    if let Some(expl) = update_plan_explanation_from_args(args) {
                        let sections = extract_high_signal_sections_from_assistant_reply(&expl);
                        for section in sections {
                            // Avoid duplicating the plan candidate; the plan steps themselves are
                            // already captured below as a dedicated `plan` hit.
                            if section.kind == "plan" {
                                continue;
                            }

                            match section.kind {
                                "decision" => {
                                    if decision_count >= 2 {
                                        continue;
                                    }
                                    decision_count = decision_count.saturating_add(1);
                                }
                                "change" => {
                                    if change_section_count >= 2 {
                                        continue;
                                    }
                                    change_section_count = change_section_count.saturating_add(1);
                                }
                                "blocker" => {
                                    if blocker_count >= 2 {
                                        continue;
                                    }
                                    blocker_count = blocker_count.saturating_add(1);
                                }
                                "evidence" => {
                                    if evidence_section_count >= 2 {
                                        continue;
                                    }
                                    evidence_section_count =
                                        evidence_section_count.saturating_add(1);
                                }
                                _ => {}
                            }

                            let title = Some(section.title.to_string());
                            let embed_text = super::build_embed_text(
                                section.kind,
                                title.as_deref(),
                                &section.body,
                                1_024,
                            );
                            let excerpt = super::trim_to_chars(
                                &embed_text,
                                super::excerpt_chars(response_mode),
                            );
                            out.push(stored_candidate(
                                section.kind,
                                title,
                                ts_ms.unwrap_or(meta.mtime_ms),
                                embed_text,
                                excerpt,
                                Some(serde_json::json!({
                                    "session_id": meta.session_id,
                                    "source": meta.source_rel,
                                    "tool": "update_plan",
                                    "section": section.title,
                                })),
                                meta,
                            ));
                        }
                    }

                    if plan_count < 2 {
                        if let Some(cand) = candidate_from_update_plan_args(
                            args,
                            meta,
                            response_mode,
                            ts_ms.unwrap_or(meta.mtime_ms),
                        ) {
                            plan_count = plan_count.saturating_add(1);
                            out.push(cand);
                        }
                    }
                } else if name == "exec_command" {
                    if command_count >= 1 {
                        continue;
                    }
                    if let Some(cand) = candidate_from_exec_command_args(
                        args,
                        meta,
                        response_mode,
                        ts_ms.unwrap_or(meta.mtime_ms),
                    ) {
                        command_count = command_count.saturating_add(1);
                        out.push(cand);
                    }
                }
            }
            "function_call_output" => {
                if tool_output_count >= 1 {
                    continue;
                }

                let text = extract_function_call_output_text(payload);
                if !is_interesting_tool_output(&text) {
                    continue;
                }

                let text = super::trim_to_chars(&text, 700);
                let title = first_line_title(&text, 90).or_else(|| Some("tool_output".to_string()));
                let embed_text =
                    super::build_embed_text("tool_output", title.as_deref(), &text, 1_024);
                let excerpt =
                    super::trim_to_chars(&embed_text, super::excerpt_chars(response_mode));

                tool_output_count = tool_output_count.saturating_add(1);
                out.push(stored_candidate(
                    "tool_output",
                    title,
                    ts_ms.unwrap_or(meta.mtime_ms),
                    embed_text,
                    excerpt,
                    Some(serde_json::json!({
                        "session_id": meta.session_id,
                        "source": meta.source_rel,
                        "output": true,
                    })),
                    meta,
                ));
            }
            "custom_tool_call" => {
                let Some(name) = payload.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if name != "apply_patch" || patch_count >= 2 {
                    continue;
                }
                let input = payload.get("input").and_then(Value::as_str).unwrap_or("");
                let paths = filter_patch_paths_for_root(project_root, extract_patch_paths(input));
                if paths.is_empty() {
                    continue;
                }
                patch_count = patch_count.saturating_add(1);

                let title = Some(format!("apply_patch: {} file(s)", paths.len()));
                let body = paths.join("\n");
                let embed_text = super::build_embed_text("change", title.as_deref(), &body, 1_024);
                let excerpt =
                    super::trim_to_chars(&embed_text, super::excerpt_chars(response_mode));
                out.push(stored_candidate(
                    "change",
                    title,
                    ts_ms.unwrap_or(meta.mtime_ms),
                    embed_text,
                    excerpt,
                    Some(serde_json::json!({
                        "session_id": meta.session_id,
                        "source": meta.source_rel,
                        "files": paths,
                    })),
                    meta,
                ));
            }
            _ => {}
        }
    }

    out.sort_by(|a, b| b.ts_ms.unwrap_or(0).cmp(&a.ts_ms.unwrap_or(0)));
    out.truncate(16);
    out
}

fn update_plan_explanation_from_args(args: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(args).ok()?;
    let expl = parsed.get("explanation").and_then(Value::as_str)?.trim();
    if expl.is_empty() {
        return None;
    }
    Some(super::trim_to_chars(expl, 4_096))
}

struct HighSignalSection {
    kind: &'static str,
    title: &'static str,
    body: String,
}

fn extract_high_signal_sections_from_assistant_reply(text: &str) -> Vec<HighSignalSection> {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum SectionTag {
        Decision,
        Change,
        Blocker,
        Plan,
        Evidence,
    }

    fn strip_markdown_heading(line: &str) -> &str {
        let mut s = line.trim();
        while s.starts_with('#') {
            s = s.trim_start_matches('#').trim_start();
        }
        while s.starts_with('*')
            || s.starts_with('_')
            || s.starts_with('-')
            || s.starts_with('•')
            || s.starts_with('—')
            || s.starts_with('–')
            || s.starts_with('─')
            || s.starts_with('>')
            || s.starts_with('|')
        {
            s = s
                .trim_start_matches(['*', '_', '-', '•', '—', '–', '─', '>', '|'])
                .trim_start();
        }
        s
    }

    fn detect_heading(line: &str) -> Option<(SectionTag, String)> {
        let cleaned = strip_markdown_heading(line).trim();
        if cleaned.is_empty() {
            return None;
        }

        // Explicit agent-friendly tags: `[decision]`, `[plan]`, `[evidence]`, etc.
        if let Some(rest) = cleaned.strip_prefix('[') {
            if let Some((tag_raw, remainder_raw)) = rest.split_once(']') {
                let head = tag_raw.trim();
                let remainder = remainder_raw
                    .trim_start()
                    .trim_start_matches([':', '-', '—', '–', '>', '|'])
                    .trim_start();
                if !head.is_empty() {
                    let upper = head.to_uppercase();
                    let tag = if matches!(upper.as_str(), "РЕШЕНИЕ" | "DECISION" | "WHY") {
                        Some(SectionTag::Decision)
                    } else if matches!(upper.as_str(), "СДЕЛАЛ" | "CHANGE" | "CHANGES") {
                        Some(SectionTag::Change)
                    } else if matches!(upper.as_str(), "БЛОКЕР" | "BLOCKER" | "RISKS" | "RISK")
                    {
                        Some(SectionTag::Blocker)
                    } else if matches!(upper.as_str(), "ПЛАН" | "PLAN" | "NEXT" | "TODO") {
                        Some(SectionTag::Plan)
                    } else if matches!(upper.as_str(), "ДОКАЗАТЕЛЬСТВА" | "EVIDENCE" | "PROOF")
                    {
                        Some(SectionTag::Evidence)
                    } else {
                        None
                    };
                    if let Some(tag) = tag {
                        return Some((tag, remainder.to_string()));
                    }
                }
            }
        }

        let mut candidates: Vec<(&str, &str)> = Vec::new();
        if let Some((h, r)) = cleaned.split_once(':') {
            candidates.push((h.trim(), r.trim()));
        }
        for sep in ["—", "–", "->", "→", "=>"] {
            if let Some((h, r)) = cleaned.split_once(sep) {
                candidates.push((h.trim(), r.trim()));
            }
        }
        if let Some((h, r)) = cleaned.split_once('-') {
            candidates.push((h.trim(), r.trim()));
        }
        candidates.push((cleaned, ""));

        for (head, remainder) in candidates {
            let upper = head.to_uppercase();
            let tag = if upper.starts_with("РЕШЕНИЕ")
                || upper.starts_with("DECISION")
                || upper.starts_with("ПРИЧИН")
                || upper.contains("ПРИЧИН")
                || upper.starts_with("ЗАЧЕМ")
                || upper.starts_with("WHY")
                || upper.contains("ROOT CAUSE")
                || upper.contains("RATIONALE")
            {
                Some(SectionTag::Decision)
            } else if upper.starts_with("СДЕЛАЛ")
                || upper.starts_with("СДЕЛАНО")
                || upper.starts_with("ИЗМЕНИЛ")
                || upper.starts_with("ИЗМЕНЕНИ")
                || upper.starts_with("ОБНОВЛ")
                || upper.starts_with("РЕАЛИЗОВ")
                || upper.starts_with("FIXED")
                || upper.starts_with("IMPLEMENTED")
                || upper.starts_with("WHAT CHANGED")
                || upper.starts_with("CHANGES")
                || upper.starts_with("CHANGELOG")
                || upper.starts_with("UPDATE")
            {
                Some(SectionTag::Change)
            } else if upper.starts_with("БЛОКЕР")
                || upper.starts_with("BLOCKER")
                || upper.starts_with("РИСК")
                || upper.starts_with("RISKS")
                || upper.starts_with("НУЖНО ОТ ТЕБЯ")
                || upper.starts_with("NEED FROM YOU")
            {
                Some(SectionTag::Blocker)
            } else if upper.starts_with("ДАЛЬШЕ")
                || upper.starts_with("NEXT")
                || upper.starts_with("NEXT STEP")
                || upper.contains("NEXT STEP")
                || upper.starts_with("СЛЕДУЮЩ")
                || upper.contains("СЛЕДУЮЩ")
                || upper.starts_with("ПЛАН")
                || upper.starts_with("PLAN")
                || upper.starts_with("КУРС")
                || upper.starts_with("TODO")
            {
                Some(SectionTag::Plan)
            } else if upper.starts_with("СТАТУС")
                || upper.starts_with("STATUS")
                || upper.starts_with("RESULT")
                || upper.starts_with("ИТОГ")
                || upper.starts_with("SUMMARY")
                || upper.starts_with("ПРУФ")
                || upper.starts_with("ДОКАЗАТЕЛЬСТВ")
                || upper.starts_with("EVIDENCE")
                || upper.starts_with("PROOF")
            {
                Some(SectionTag::Evidence)
            } else {
                None
            };
            if let Some(tag) = tag {
                return Some((tag, remainder.to_string()));
            }
        }
        None
    }

    let mut current: Option<SectionTag> = None;
    let mut decision: Vec<String> = Vec::new();
    let mut change: Vec<String> = Vec::new();
    let mut blocker: Vec<String> = Vec::new();
    let mut plan: Vec<String> = Vec::new();
    let mut evidence: Vec<String> = Vec::new();

    for line in text.lines() {
        if let Some((tag, remainder)) = detect_heading(line) {
            current = Some(tag);
            if !remainder.trim().is_empty() {
                match tag {
                    SectionTag::Decision => decision.push(remainder),
                    SectionTag::Change => change.push(remainder),
                    SectionTag::Blocker => blocker.push(remainder),
                    SectionTag::Plan => plan.push(remainder),
                    SectionTag::Evidence => evidence.push(remainder),
                }
            }
            continue;
        }

        let Some(tag) = current else { continue };
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        match tag {
            SectionTag::Decision => decision.push(line.to_string()),
            SectionTag::Change => change.push(line.to_string()),
            SectionTag::Blocker => blocker.push(line.to_string()),
            SectionTag::Plan => plan.push(line.to_string()),
            SectionTag::Evidence => evidence.push(line.to_string()),
        }
    }

    let mut out = Vec::new();
    let decision_body = decision.join("\n").trim().to_string();
    if !decision_body.is_empty() {
        out.push(HighSignalSection {
            kind: "decision",
            title: "decision",
            body: super::trim_to_chars(&decision_body, 700),
        });
    }
    let change_body = change.join("\n").trim().to_string();
    if !change_body.is_empty() {
        out.push(HighSignalSection {
            kind: "change",
            title: "change",
            body: super::trim_to_chars(&change_body, 700),
        });
    }
    let blocker_body = blocker.join("\n").trim().to_string();
    if !blocker_body.is_empty() {
        out.push(HighSignalSection {
            kind: "blocker",
            title: "blocker",
            body: super::trim_to_chars(&blocker_body, 500),
        });
    }
    let plan_body = plan.join("\n").trim().to_string();
    if !plan_body.is_empty() {
        out.push(HighSignalSection {
            kind: "plan",
            title: "next",
            body: super::trim_to_chars(&plan_body, 500),
        });
    }
    let evidence_body = evidence.join("\n").trim().to_string();
    if !evidence_body.is_empty() {
        out.push(HighSignalSection {
            kind: "evidence",
            title: "proof",
            body: super::trim_to_chars(&evidence_body, 450),
        });
    }

    if !out.is_empty() {
        return out;
    }

    extract_implicit_signal_sections_from_assistant_reply(text)
}

fn extract_implicit_signal_sections_from_assistant_reply(text: &str) -> Vec<HighSignalSection> {
    #[derive(Clone, Debug)]
    struct LineScore {
        idx: usize,
        text: String,
        decision: u32,
        plan: u32,
        evidence: u32,
        blocker: u32,
        change: u32,
    }

    fn strip_code_fences(text: &str) -> String {
        let mut out = String::new();
        let mut in_code = false;
        for line in text.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                in_code = !in_code;
                continue;
            }
            if in_code {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line);
        }
        out
    }

    fn normalize_signal_line(raw: &str) -> Option<String> {
        let line = raw.trim();
        if line.is_empty() {
            return None;
        }
        let line = line
            .trim_start_matches(['*', '_', '-', '•', '—', '–', '─', '>', '|'])
            .trim();
        let line = line.trim();
        if line.is_empty() {
            return None;
        }

        // Drop pure file-path / log-noise lines.
        if !line.contains(' ')
            && (line.ends_with(".rs")
                || line.ends_with(".ts")
                || line.ends_with(".js")
                || line.ends_with(".json")
                || line.ends_with(".toml")
                || line.ends_with(".yaml")
                || line.ends_with(".yml")
                || line.ends_with(".md"))
        {
            return None;
        }

        Some(super::trim_to_chars(line, 320))
    }

    fn looks_like_raw_command(line: &str) -> bool {
        let trimmed = line.trim_start();
        if trimmed.starts_with('$') {
            return true;
        }
        let lower = trimmed.to_lowercase();
        for prefix in [
            "cargo ", "rg ", "ps ", "nohup ", "kill ", "cd ", "ls ", "git ", "python ", "python3 ",
        ] {
            if lower.starts_with(prefix) {
                return true;
            }
        }
        trimmed.contains("&&") || trimmed.contains(" | ")
    }

    fn score_contains_any(lower: &str, needles: &[(&str, u32)]) -> u32 {
        let mut score = 0u32;
        for (needle, weight) in needles {
            if lower.contains(needle) {
                score = score.saturating_add(*weight);
            }
        }
        score
    }

    let cleaned = strip_code_fences(text);
    let mut lines: Vec<LineScore> = Vec::new();
    for (idx, raw) in cleaned.lines().enumerate() {
        let Some(line) = normalize_signal_line(raw) else {
            continue;
        };
        let lower = line.to_lowercase();
        if lower.contains("<environment_context>") || lower.contains("<instructions>") {
            continue;
        }

        let mut decision = score_contains_any(
            &lower,
            &[
                ("решени", 4),
                ("decision", 4),
                ("root cause", 4),
                ("rationale", 4),
                ("потому", 2),
                ("поэтому", 2),
                ("выбира", 2),
                ("делаем", 2),
                ("будем", 1),
                ("approach", 2),
                ("strategy", 2),
            ],
        );
        if lower.contains('?') && decision < 6 {
            decision = 0;
        }

        let plan = score_contains_any(
            &lower,
            &[
                ("дальше", 4),
                ("следующ", 3),
                ("next step", 4),
                ("next", 2),
                ("todo", 3),
                ("план", 2),
                ("plan", 2),
                ("провер", 2),
                ("verify", 2),
                ("ship", 2),
                ("в течение", 1),
            ],
        );

        let evidence = score_contains_any(
            &lower,
            &[
                ("доказател", 4),
                ("evidence", 4),
                ("proof", 4),
                ("пруф", 4),
                ("verified", 3),
                ("проверил", 2),
                ("confirmed", 2),
                ("works", 2),
                ("гейты", 3),
                ("green", 2),
                ("tests", 3),
                ("clippy", 3),
                ("fmt", 2),
                ("doctor", 3),
                ("issues=0", 4),
                ("issues = 0", 4),
                ("hints=0", 3),
            ],
        );

        let blocker = score_contains_any(
            &lower,
            &[
                ("блокер", 4),
                ("blocker", 4),
                ("нужно от тебя", 4),
                ("need from you", 4),
                ("нужен доступ", 4),
                ("нет доступа", 4),
                ("requires", 2),
                ("approval", 2),
                ("key", 2),
                ("secret", 2),
            ],
        );

        let change = score_contains_any(
            &lower,
            &[
                ("сделал", 3),
                ("сделано", 3),
                ("починил", 3),
                ("fixed", 3),
                ("implemented", 3),
                ("added", 2),
                ("updated", 2),
                ("реализ", 2),
                ("обновл", 2),
                ("изменил", 2),
            ],
        );

        // Avoid turning raw command lines into "signal".
        if looks_like_raw_command(&line) && decision < 6 && plan < 6 && evidence < 6 {
            continue;
        }

        lines.push(LineScore {
            idx,
            text: line,
            decision,
            plan,
            evidence,
            blocker,
            change,
        });
    }

    if lines.is_empty() {
        return Vec::new();
    }

    fn pick_lines(
        lines: &[LineScore],
        score_of: fn(&LineScore) -> u32,
        threshold: u32,
        limit: usize,
        used: &mut HashSet<String>,
    ) -> Vec<String> {
        let mut candidates: Vec<&LineScore> =
            lines.iter().filter(|l| score_of(l) >= threshold).collect();
        candidates.sort_by(|a, b| {
            score_of(b)
                .cmp(&score_of(a))
                .then_with(|| a.idx.cmp(&b.idx))
        });
        let mut out = Vec::new();
        for cand in candidates {
            if out.len() >= limit {
                break;
            }
            if used.contains(&cand.text) {
                continue;
            }
            used.insert(cand.text.clone());
            out.push(cand.text.clone());
        }
        out
    }

    let mut used: HashSet<String> = HashSet::new();
    let decision_lines = pick_lines(&lines, |l| l.decision, 4, 3, &mut used);
    let blocker_lines = pick_lines(&lines, |l| l.blocker, 4, 2, &mut used);
    let plan_lines = pick_lines(&lines, |l| l.plan, 4, 3, &mut used);
    let evidence_lines = pick_lines(&lines, |l| l.evidence, 4, 3, &mut used);
    let change_lines = pick_lines(&lines, |l| l.change, 4, 3, &mut used);

    let mut out = Vec::new();
    if !decision_lines.is_empty() {
        out.push(HighSignalSection {
            kind: "decision",
            title: "decision",
            body: super::trim_to_chars(&decision_lines.join("\n"), 700),
        });
    }
    if !blocker_lines.is_empty() {
        out.push(HighSignalSection {
            kind: "blocker",
            title: "blocker",
            body: super::trim_to_chars(&blocker_lines.join("\n"), 500),
        });
    }
    if !plan_lines.is_empty() {
        out.push(HighSignalSection {
            kind: "plan",
            title: "next",
            body: super::trim_to_chars(&plan_lines.join("\n"), 500),
        });
    }
    if !evidence_lines.is_empty() {
        out.push(HighSignalSection {
            kind: "evidence",
            title: "proof",
            body: super::trim_to_chars(&evidence_lines.join("\n"), 450),
        });
    }
    if !change_lines.is_empty() {
        out.push(HighSignalSection {
            kind: "change",
            title: "change",
            body: super::trim_to_chars(&change_lines.join("\n"), 700),
        });
    }
    out
}

fn candidate_from_user_requirements(
    text: &str,
    meta: &CodexSessionMeta,
    response_mode: ResponseMode,
    ts_ms: u64,
) -> Option<StoredCandidate> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw
            .trim()
            .trim_start_matches(['•', '-', '*', '—', '–', '─'])
            .trim();
        if line.is_empty() {
            continue;
        }
        if line.len() > 260 {
            continue;
        }
        if !looks_like_requirement_line(line) {
            continue;
        }
        lines.push(line.to_string());
        if lines.len() >= 8 {
            break;
        }
    }

    if lines.is_empty() {
        // Single-paragraph prompts are common; do a simple sentence split as a fallback.
        let mut sentence = String::new();
        for ch in text.chars() {
            sentence.push(ch);
            if matches!(ch, '.' | '!' | '?' | '\n') {
                let candidate = sentence.trim();
                if !candidate.is_empty()
                    && candidate.len() <= 260
                    && looks_like_requirement_line(candidate)
                {
                    lines.push(candidate.to_string());
                    if lines.len() >= 8 {
                        break;
                    }
                }
                sentence.clear();
            }
        }
    }

    if lines.is_empty() {
        return None;
    }

    let body = lines.join("\n");
    let title = Some("requirements".to_string());
    let embed_text = super::build_embed_text("requirement", title.as_deref(), &body, 1_024);
    let excerpt = super::trim_to_chars(&embed_text, super::excerpt_chars(response_mode));

    Some(stored_candidate(
        "requirement",
        title,
        ts_ms,
        embed_text,
        excerpt,
        Some(serde_json::json!({
            "session_id": meta.session_id,
            "source": meta.source_rel,
            "role": "user",
        })),
        meta,
    ))
}

fn looks_like_requirement_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    if lower.contains("<instructions>") || lower.contains("<environment_context>") {
        return false;
    }

    // Strong constraints / invariants we want agents to remember.
    let mut hits = 0u32;
    for needle in [
        "должн",
        "нужно",
        "необходимо",
        "обязательно",
        "требу",
        "нельзя",
        "никак",
        "без руч",
        "вручн",
        "автомат",
        "когнитивно",
        "всегда",
        "никогда",
        "must",
        "should",
        "always",
        "never",
        "no manual",
        "automatically",
    ] {
        if lower.contains(needle) {
            hits = hits.saturating_add(1);
        }
    }

    // Prefer declarative constraints over simple questions.
    if hits == 0 {
        return false;
    }
    if lower.contains('?') && hits < 2 {
        return false;
    }
    true
}

fn extract_function_call_output_text(payload: &Value) -> String {
    fn normalize_output_value(value: &Value) -> Option<String> {
        match value {
            Value::String(s) => Some(s.clone()),
            Value::Array(items) => {
                let mut lines = Vec::new();
                for item in items {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        let text = text.trim();
                        if !text.is_empty() {
                            lines.push(text.to_string());
                        }
                    }
                }
                (!lines.is_empty()).then(|| lines.join("\n"))
            }
            Value::Object(_) => serde_json::to_string(value).ok(),
            _ => None,
        }
    }

    let Some(raw) = payload.get("output") else {
        return String::new();
    };

    if let Some(mut text) = normalize_output_value(raw) {
        let trimmed = text.trim();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                if let Some(parsed_text) = normalize_output_value(&parsed) {
                    text = parsed_text;
                }
            }
        }
        return text;
    }

    String::new()
}

fn is_interesting_tool_output(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let first = trimmed.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return false;
    }
    let lower = first.to_lowercase();
    if matches!(lower.as_str(), "plan updated" | "ok" | "done") {
        return false;
    }

    lower.starts_with("ok:")
        || lower.starts_with("err:")
        || lower.starts_with("error:")
        || lower.contains("failed")
        || lower.contains("panic")
        || lower.contains("mcp error")
}

fn candidate_from_exec_command_args(
    args: &str,
    meta: &CodexSessionMeta,
    response_mode: ResponseMode,
    ts_ms: u64,
) -> Option<StoredCandidate> {
    let parsed: Value = serde_json::from_str(args).ok()?;
    let cmd = parsed.get("cmd")?.as_str()?.trim();
    let cmd = sanitize_exec_command(cmd)?;

    let title = Some(cmd.clone());
    let embed_text = super::build_embed_text("command", title.as_deref(), "", 256);
    let excerpt = super::trim_to_chars(&embed_text, super::excerpt_chars(response_mode));

    Some(stored_candidate(
        "command",
        title,
        ts_ms,
        embed_text,
        excerpt,
        Some(serde_json::json!({
            "session_id": meta.session_id,
            "source": meta.source_rel,
            "tool": "exec_command",
        })),
        meta,
    ))
}

fn sanitize_exec_command(cmd: &str) -> Option<String> {
    let raw = cmd.trim();
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_lowercase();
    if lower.contains("token")
        || lower.contains("secret")
        || lower.contains("apikey")
        || lower.contains("api_key")
        || lower.contains("password")
        || lower.contains("authorization")
    {
        return None;
    }
    for risky in ["curl ", "wget ", "ssh ", "scp ", "aws ", "gcloud "] {
        if lower.starts_with(risky) {
            return None;
        }
    }

    // Drop leading `cd ... &&` noise (common in Codex logs).
    let simplified = if let Some((_, last)) = raw.rsplit_once("&&") {
        last.trim()
    } else {
        raw
    };

    let simplified = simplified
        .trim()
        .trim_matches(|c: char| c == ';' || c == '&');

    if simplified.is_empty() {
        return None;
    }
    Some(super::trim_to_chars(simplified, 160))
}

fn extract_message_text(payload: &Value) -> String {
    let mut out = String::new();
    let Some(content) = payload.get("content").and_then(Value::as_array) else {
        return out;
    };
    for item in content {
        let Some(kind) = item.get("type").and_then(Value::as_str) else {
            continue;
        };
        if kind != "input_text" {
            continue;
        }
        let Some(text) = item.get("text").and_then(Value::as_str) else {
            continue;
        };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(text);
    }
    out.trim().to_string()
}

fn is_noise_transcript_message(role: &str, text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }

    let lower = trimmed.to_lowercase();
    if lower.contains("<environment_context>") {
        return true;
    }
    if lower.contains("<instructions>") && lower.contains("## skills") {
        return true;
    }
    if lower.starts_with("# agents.md instructions") {
        return true;
    }

    if trimmed.chars().count() <= 12 && !trimmed.contains('?') && !trimmed.contains('\n') {
        let tiny = lower.replace(|c: char| !c.is_alphanumeric(), "");
        if matches!(
            tiny.as_str(),
            "ok" | "okay" | "да" | "ага" | "угу" | "yes" | "no" | "нет" | "продолжай"
        ) {
            return true;
        }
        if role == "user" {
            return true;
        }
    }

    false
}

fn first_line_title(text: &str, max_chars: usize) -> Option<String> {
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return None;
    }
    Some(super::trim_to_chars(line, max_chars))
}

fn stored_candidate(
    kind: &str,
    title: Option<String>,
    ts_ms: u64,
    embed_text: String,
    excerpt: String,
    reference: Option<Value>,
    meta: &CodexSessionMeta,
) -> StoredCandidate {
    let content_sha256 = sha256_hex(embed_text.as_bytes());
    let semantic_key = semantic_key_for_candidate(kind, &embed_text);
    StoredCandidate {
        kind: kind.to_string(),
        title,
        ts_ms: Some(ts_ms),
        embed_text,
        excerpt,
        reference,
        content_sha256,
        semantic_key,
        session_id: meta.session_id.clone(),
        source_rel: meta.source_rel.clone(),
    }
}

fn merge_stored_candidate(
    existing: &mut StoredCandidate,
    incoming: StoredCandidate,
    response_mode: ResponseMode,
) {
    let existing_ts = existing.ts_ms.unwrap_or(0);
    let incoming_ts = incoming.ts_ms.unwrap_or(0);

    // Prefer newer candidates; when timestamps tie, keep the denser one.
    let prefer_incoming = incoming_ts > existing_ts
        || (incoming_ts == existing_ts && incoming.embed_text.len() > existing.embed_text.len());

    let (mut base, other) = if prefer_incoming {
        (incoming, existing.clone())
    } else {
        (existing.clone(), incoming)
    };

    merge_reference(&mut base.reference, other.reference);
    base.ts_ms = Some(existing_ts.max(incoming_ts));

    match base.kind.trim().to_ascii_lowercase().as_str() {
        "decision" => {
            let body = merge_decision_bodies(&base.embed_text, &other.embed_text);
            base.embed_text =
                super::build_embed_text(&base.kind, base.title.as_deref(), &body, 1_024);
        }
        // For plans, treat the newest candidate as "truth" (statuses are stateful). The semantic
        // key ignores status churn, so duplicates collapse naturally while keeping the latest view.
        "plan" => {}
        _ => {}
    }

    // Keep derived fields consistent.
    base.excerpt = super::trim_to_chars(&base.embed_text, super::excerpt_chars(response_mode));
    base.content_sha256 = sha256_hex(base.embed_text.as_bytes());
    base.semantic_key = semantic_key_for_candidate(&base.kind, &base.embed_text);

    *existing = base;
}

fn merge_reference(existing: &mut Option<Value>, incoming: Option<Value>) {
    let Some(incoming) = incoming else {
        return;
    };

    let Some(existing_value) = existing.take() else {
        *existing = Some(incoming);
        return;
    };

    fn as_sources(value: Value) -> Vec<Value> {
        match value {
            Value::Object(map) => map
                .get("sources")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_else(|| vec![Value::Object(map)]),
            other => vec![other],
        }
    }

    let mut sources = as_sources(existing_value);
    sources.extend(as_sources(incoming));

    let mut seen: HashSet<String> = HashSet::new();
    sources.retain(|v| {
        let key = serde_json::to_string(v).unwrap_or_default();
        seen.insert(key)
    });
    sources.truncate(4);

    *existing = Some(serde_json::json!({ "sources": sources }));
}

fn embed_text_body(embed_text: &str) -> &str {
    embed_text
        .split_once("\n\n")
        .map(|(_, rest)| rest)
        .unwrap_or("")
}

fn merge_decision_bodies(base_embed_text: &str, other_embed_text: &str) -> String {
    let base_body = embed_text_body(base_embed_text);
    let other_body = embed_text_body(other_embed_text);

    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    fn push_lines(lines: &mut Vec<String>, seen: &mut HashSet<String>, text: &str) {
        for raw in text.lines() {
            let line = raw.trim_end();
            if line.trim().is_empty() {
                continue;
            }
            let key = normalize_semantic_text(line);
            if key.is_empty() {
                continue;
            }
            if !seen.insert(key) {
                continue;
            }
            lines.push(line.to_string());
            if lines.len() >= 20 {
                break;
            }
        }
    }

    // Preserve "latest" narrative order but avoid losing details that were only present in older
    // decision blocks.
    push_lines(&mut out, &mut seen, base_body);
    if out.len() < 20 {
        push_lines(&mut out, &mut seen, other_body);
    }

    if out.is_empty() {
        // Defensive fallback for unexpected payloads without `\n\n`.
        let fallback = if base_body.trim().is_empty() {
            other_body
        } else {
            base_body
        };
        return super::trim_to_chars(fallback, 900);
    }

    let merged = out.join("\n");
    super::trim_to_chars(&merged, 900)
}

fn semantic_key_for_candidate(kind: &str, embed_text: &str) -> Option<String> {
    let kind = kind.trim().to_ascii_lowercase();
    let body = embed_text
        .split_once("\n\n")
        .map(|(_, rest)| rest)
        .unwrap_or(embed_text);
    let body = body.trim();
    if body.is_empty() {
        return None;
    }

    let normalized = match kind.as_str() {
        "decision" => {
            let head = body
                .lines()
                .map(|l| l.trim())
                .find(|l| !l.is_empty())
                .unwrap_or(body);
            normalize_semantic_text(head)
        }
        "plan" => normalize_plan_semantics(body).unwrap_or_else(|| normalize_semantic_text(body)),
        _ => return None,
    };

    if normalized.is_empty() {
        return None;
    }

    let payload = format!("semkey:v2\n{kind}\n{normalized}");
    Some(sha256_hex(payload.as_bytes()))
}

fn normalize_plan_semantics(body: &str) -> Option<String> {
    let mut steps: Vec<String> = Vec::new();
    for raw in body.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Prefer bullet-style plan steps; free-form plans fall back to whole-body normalization.
        if !matches!(
            trimmed.chars().next(),
            Some('•' | '-' | '*' | '—' | '–' | '─')
        ) {
            continue;
        }

        let mut line = trimmed;
        if line.is_empty() {
            continue;
        }
        // Prefer plan "steps" (bullets) over explanations, and strip status churn like
        // `- (completed) Step` so the key stays stable.
        line = line
            .trim_start_matches(['•', '-', '*', '—', '–', '─'])
            .trim_start();
        if line.starts_with('(') {
            if let Some((_, rest)) = line.split_once(')') {
                line = rest.trim_start();
            }
        }
        if line.is_empty() {
            continue;
        }
        steps.push(line.to_string());
        if steps.len() >= 16 {
            break;
        }
    }
    if steps.is_empty() {
        return None;
    }
    let joined = steps.join("\n");
    Some(normalize_semantic_text(&joined))
}

fn normalize_semantic_text(input: &str) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for ch in input.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

fn candidate_from_update_plan_args(
    args: &str,
    meta: &CodexSessionMeta,
    response_mode: ResponseMode,
    ts_ms: u64,
) -> Option<StoredCandidate> {
    let parsed: Value = serde_json::from_str(args).ok()?;
    let plan = parsed.get("plan")?.as_array()?;
    if plan.is_empty() {
        return None;
    }

    let mut lines: Vec<String> = Vec::new();
    if let Some(expl) = parsed.get("explanation").and_then(Value::as_str) {
        let expl = expl.trim();
        if !expl.is_empty() {
            lines.push(expl.to_string());
        }
    }
    for item in plan.iter().take(8) {
        let step = item
            .get("step")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let status = item
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if step.is_empty() {
            continue;
        }
        if status.is_empty() {
            lines.push(format!("- {step}"));
        } else {
            lines.push(format!("- ({status}) {step}"));
        }
    }
    if lines.is_empty() {
        return None;
    }

    let body = lines.join("\n");
    let title = Some("update_plan".to_string());
    let embed_text = super::build_embed_text("plan", title.as_deref(), &body, 1_024);
    let excerpt = super::trim_to_chars(&embed_text, super::excerpt_chars(response_mode));

    Some(stored_candidate(
        "plan",
        title,
        ts_ms,
        embed_text,
        excerpt,
        Some(serde_json::json!({
            "session_id": meta.session_id,
            "source": meta.source_rel,
        })),
        meta,
    ))
}

fn extract_patch_paths(patch: &str) -> Vec<String> {
    const MAX_FILES: usize = 32;
    let mut out: Vec<String> = Vec::new();
    for line in patch.lines() {
        let line = line.trim();
        let path = line
            .strip_prefix("*** Add File: ")
            .or_else(|| line.strip_prefix("*** Update File: "))
            .or_else(|| line.strip_prefix("*** Delete File: "))
            .or_else(|| line.strip_prefix("*** Move to: "));
        let Some(path) = path else {
            continue;
        };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        out.push(path.to_string());
        if out.len() >= MAX_FILES {
            break;
        }
    }
    out.sort();
    out.dedup();
    out
}

fn filter_patch_paths_for_root(project_root: &Path, paths: Vec<String>) -> Vec<String> {
    let mut filtered: Vec<String> = paths
        .into_iter()
        .filter_map(|raw| scoped_patch_path(project_root, &raw))
        .collect();
    filtered.sort();
    filtered.dedup();
    filtered
}

fn scoped_patch_path(project_root: &Path, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let path = Path::new(raw);
    if path.is_absolute() {
        // Keep only paths rooted inside the current project. Use repo-relative form to keep memory
        // overlays project-scoped and to avoid cross-project “leakage” in read_pack memory packs.
        if !path.starts_with(project_root) {
            return None;
        }
        let rel = path.strip_prefix(project_root).ok()?;
        let rel = normalize_rel_components(rel);
        return if rel.is_empty() { None } else { Some(rel) };
    }

    // Reject traversal / absolute-ish relative paths. Even if a session started under this
    // project root, we do not want memory overlays to surface edits to sibling repos.
    if path.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return None;
    }

    // Best-effort: if the target exists, canonicalize to enforce root scoping through symlinks.
    let joined = project_root.join(path);
    if let Ok(canon) = joined.canonicalize() {
        if !canon.starts_with(project_root) {
            return None;
        }
        if let Ok(rel) = canon.strip_prefix(project_root) {
            let rel = normalize_rel_components(rel);
            return if rel.is_empty() { None } else { Some(rel) };
        }
    }

    let rel = normalize_rel_components(path);
    if rel.is_empty() {
        None
    } else {
        Some(rel)
    }
}

fn normalize_rel_components(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(seg) => parts.push(seg.to_string_lossy().to_string()),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                // The caller should have filtered these out already; ignore defensively.
            }
        }
    }
    parts.join("/")
}

fn list_recent_rollout_jsonl_sync(sessions_root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let Ok(mut years) = std::fs::read_dir(sessions_root) else {
        return out;
    };

    let mut year_dirs: Vec<(u32, PathBuf)> = Vec::new();
    while let Some(Ok(entry)) = years.next() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.len() == 4 && name.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = name.parse::<u32>() {
                year_dirs.push((n, entry.path()));
            }
        }
    }
    year_dirs.sort_by(|a, b| b.0.cmp(&a.0));

    for (_, year_path) in year_dirs {
        let Ok(mut months) = std::fs::read_dir(&year_path) else {
            continue;
        };
        let mut month_dirs: Vec<(u32, PathBuf)> = Vec::new();
        while let Some(Ok(entry)) = months.next() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.len() == 2 && name.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(n) = name.parse::<u32>() {
                    month_dirs.push((n, entry.path()));
                }
            }
        }
        month_dirs.sort_by(|a, b| b.0.cmp(&a.0));

        for (_, month_path) in month_dirs {
            let Ok(mut days) = std::fs::read_dir(&month_path) else {
                continue;
            };
            let mut day_dirs: Vec<(u32, PathBuf)> = Vec::new();
            while let Some(Ok(entry)) = days.next() {
                let Ok(ft) = entry.file_type() else {
                    continue;
                };
                if !ft.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if name.len() == 2 && name.chars().all(|c| c.is_ascii_digit()) {
                    if let Ok(n) = name.parse::<u32>() {
                        day_dirs.push((n, entry.path()));
                    }
                }
            }
            day_dirs.sort_by(|a, b| b.0.cmp(&a.0));

            for (_, day_path) in day_dirs {
                let Ok(mut files) = std::fs::read_dir(&day_path) else {
                    continue;
                };
                let mut rollouts: Vec<PathBuf> = Vec::new();
                while let Some(Ok(entry)) = files.next() {
                    let Ok(ft) = entry.file_type() else {
                        continue;
                    };
                    if !ft.is_file() {
                        continue;
                    }
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
                        continue;
                    }
                    rollouts.push(entry.path());
                }
                rollouts.sort_by(|a, b| {
                    b.file_name()
                        .and_then(|n| n.to_str())
                        .cmp(&a.file_name().and_then(|n| n.to_str()))
                });
                for path in rollouts {
                    out.push(path);
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
    }

    out
}

async fn list_recent_rollout_jsonl(sessions_root: &Path, limit: usize) -> Vec<PathBuf> {
    let sessions_root = sessions_root.to_path_buf();
    tokio::task::spawn_blocking(move || list_recent_rollout_jsonl_sync(&sessions_root, limit))
        .await
        .unwrap_or_default()
}

fn load_cache_sync(path: &Path) -> Option<CodexSessionsCacheV1> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

async fn load_cache(path: &Path) -> Option<CodexSessionsCacheV1> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || load_cache_sync(&path))
        .await
        .ok()
        .flatten()
}

async fn write_cache(path: &Path, cache: &CodexSessionsCacheV1) -> Option<()> {
    let parent = path.parent()?;
    tokio::fs::create_dir_all(parent).await.ok()?;
    let bytes = serde_json::to_vec_pretty(cache).ok()?;
    tokio::fs::write(path, bytes).await.ok()?;
    Some(())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|dur| dur.as_millis() as u64)
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len().saturating_mul(2));
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{:02x}", byte);
    }
    out
}

fn codex_home_dir_for_project(project_root: &Path) -> Option<PathBuf> {
    if let Ok(val) = std::env::var("CODEX_HOME") {
        let path = PathBuf::from(val);
        if !path.as_os_str().is_empty() && codex_sessions_root(&path).is_some() {
            return Some(path);
        }
    }

    if let Some(home) = dirs::home_dir() {
        let path = home.join(".codex");
        if codex_sessions_root(&path).is_some() {
            return Some(path);
        }
    }

    // Some MCP runtimes run the toolchain under a different account (e.g. root), even when the
    // project is owned by a normal user. Prefer the project owner's Codex home in that case so
    // "external memory" remains zero-config and project-scoped.
    #[cfg(unix)]
    if let Some(owner_home) = project_owner_home_dir(project_root) {
        let path = owner_home.join(".codex");
        if codex_sessions_root(&path).is_some() {
            return Some(path);
        }
    }

    None
}

fn codex_sessions_root(codex_home: &Path) -> Option<PathBuf> {
    let sessions = codex_home.join("sessions");
    sessions.is_dir().then_some(sessions)
}

#[cfg(unix)]
fn project_owner_home_dir(project_root: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let meta = std::fs::metadata(project_root).ok()?;
    let uid = meta.uid();
    home_dir_for_uid(uid)
}

#[cfg(unix)]
fn home_dir_for_uid(uid: u32) -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::ptr;

    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = ptr::null_mut();

    // POSIX recommends sysconf(_SC_GETPW_R_SIZE_MAX) but it can return -1; use a sane cap.
    let mut buf_len: usize = unsafe {
        let size = libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX);
        if size > 0 {
            usize::try_from(size).unwrap_or(16 * 1024)
        } else {
            16 * 1024
        }
    };
    buf_len = buf_len.clamp(8 * 1024, 256 * 1024);

    let mut buf: Vec<u8> = vec![0u8; buf_len];
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
        return None;
    }

    let home = unsafe { CStr::from_ptr(pwd.pw_dir) }
        .to_string_lossy()
        .to_string();
    let home = home.trim();
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home))
}

async fn cache_path_for_project(codex_home: &Path, project_root: &Path) -> Option<PathBuf> {
    let canonical = tokio::fs::canonicalize(project_root)
        .await
        .unwrap_or_else(|_| project_root.to_path_buf());
    let key_full = sha256_hex(canonical.to_string_lossy().as_bytes());
    let key = key_full.chars().take(16).collect::<String>();
    let preferred = codex_home.join(CONTEXT_DIR_NAME);
    let base = if preferred.exists() {
        preferred
    } else {
        let legacy = codex_home.join(LEGACY_CONTEXT_DIR_NAME);
        if legacy.exists() {
            legacy
        } else {
            preferred
        }
    };
    Some(
        base.join("external_memory")
            .join("codex_cli")
            .join(format!("sessions_{key}.json")),
    )
}

fn parse_rfc3339_utc_ms(ts: &str) -> Option<u64> {
    // Fast path for `YYYY-MM-DDTHH:MM:SS(.mmm)?Z` (UTC only).
    let ts = ts.trim();
    if !ts.ends_with('Z') {
        return None;
    }
    let ts = &ts[..ts.len().saturating_sub(1)];

    let (date, time) = ts.split_once('T')?;
    let (y, m, d) = {
        let mut it = date.split('-');
        let y: i32 = it.next()?.parse().ok()?;
        let m: u32 = it.next()?.parse().ok()?;
        let d: u32 = it.next()?.parse().ok()?;
        (y, m, d)
    };

    let (hh, mm, ss, ms) = {
        let (hms, frac) = match time.split_once('.') {
            Some((hms, frac)) => (hms, Some(frac)),
            None => (time, None),
        };
        let mut it = hms.split(':');
        let hh: i64 = it.next()?.parse().ok()?;
        let mm: i64 = it.next()?.parse().ok()?;
        let ss: i64 = it.next()?.parse().ok()?;
        let ms: i64 = frac
            .and_then(|v| v.get(..3))
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        (hh, mm, ss, ms)
    };

    let days = days_from_civil(y, m, d)?;
    let seconds = days
        .saturating_mul(86_400)
        .saturating_add(hh.saturating_mul(3_600))
        .saturating_add(mm.saturating_mul(60))
        .saturating_add(ss);
    let millis = seconds.saturating_mul(1_000).saturating_add(ms);
    u64::try_from(millis).ok()
}

fn days_from_civil(y: i32, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }

    // Howard Hinnant's algorithm: days since 1970-01-01.
    let y = i64::from(y);
    let m = i64::from(m);
    let d = i64::from(d);

    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rfc3339_utc_ms_parses_known_timestamp() {
        let ms = parse_rfc3339_utc_ms("1970-01-01T00:00:00.000Z").unwrap();
        assert_eq!(ms, 0);
    }
}

#[cfg(test)]
mod codex_tests {
    use super::*;
    use std::sync::{Arc, OnceLock};
    use tokio::sync::{OwnedSemaphorePermit, Semaphore};

    static CODEX_HOME_MUTEX: OnceLock<Arc<Semaphore>> = OnceLock::new();

    async fn lock_codex_home() -> OwnedSemaphorePermit {
        CODEX_HOME_MUTEX
            .get_or_init(|| Arc::new(Semaphore::new(1)))
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore closed")
    }

    struct EnvVarGuard {
        key: &'static str,
        saved: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let saved = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, saved }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.saved.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[tokio::test]
    async fn codex_overlay_recent_is_project_scoped_and_bounded() -> anyhow::Result<()> {
        let _permit = lock_codex_home().await;
        let tmp = tempfile::tempdir()?;
        let codex_home = tmp.path().join("codex_home");
        let sessions_root = codex_home.join("sessions/2026/01/06");
        tokio::fs::create_dir_all(&sessions_root).await?;

        let project = tmp.path().join("work/repo");
        tokio::fs::create_dir_all(&project).await?;

        let session = sessions_root
            .join("rollout-2026-01-06T12-00-00-00000000-0000-0000-0000-000000000000.jsonl");

        let cwd = project.to_string_lossy().to_string();
        let session_meta = serde_json::json!({
            "timestamp": "2026-01-06T12:00:00.000Z",
            "type": "session_meta",
            "payload": { "id": "s1", "timestamp": "2026-01-06T12:00:00.000Z", "cwd": cwd }
        });
        let user = serde_json::json!({
            "timestamp": "2026-01-06T12:00:01.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "сделай инкрементальную индексацию" }]
            }
        });
        let update_args = serde_json::json!({
            "plan": [{ "step": "Do X", "status": "in_progress" }]
        })
        .to_string();
        let update_plan = serde_json::json!({
            "timestamp": "2026-01-06T12:00:02.000Z",
            "type": "response_item",
            "payload": { "type": "function_call", "name": "update_plan", "arguments": update_args }
        });
        let jsonl = format!("{session_meta}\n{user}\n{update_plan}\n");
        tokio::fs::write(&session, jsonl).await?;

        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.as_os_str());
        let overlay = overlay_recent(&project, ResponseMode::Facts).await;

        let Some(overlay) = overlay else {
            anyhow::bail!("expected codex overlay");
        };
        assert_eq!(overlay.source, "codex_cli");
        assert!(overlay.hits.len() <= 3);
        Ok(())
    }

    #[tokio::test]
    async fn codex_extracts_user_requirements_and_assistant_change_sections() -> anyhow::Result<()>
    {
        let _permit = lock_codex_home().await;
        let tmp = tempfile::tempdir()?;
        let codex_home = tmp.path().join("codex_home");
        let sessions_root = codex_home.join("sessions/2026/01/06");
        tokio::fs::create_dir_all(&sessions_root).await?;

        let project = tmp.path().join("work/repo");
        tokio::fs::create_dir_all(&project).await?;

        let session = sessions_root
            .join("rollout-2026-01-06T12-01-00-00000000-0000-0000-0000-000000000000.jsonl");

        let jsonl = format!(
            "{{\"timestamp\":\"2026-01-06T12:01:00.000Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"s2\",\"timestamp\":\"2026-01-06T12:01:00.000Z\",\"cwd\":\"{}\"}}}}\n\
             {{\"timestamp\":\"2026-01-06T12:01:01.000Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"ничего не должно указываться вручную; всё должно быть автоматически\"}}]}}}}\n\
             {{\"timestamp\":\"2026-01-06T12:01:02.000Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"input_text\",\"text\":\"Сделал polite governor\\n- crates/mcp-server/src/index_warmup.rs\"}}]}}}}\n",
            project.to_string_lossy()
        );
        tokio::fs::write(&session, jsonl).await?;

        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.as_os_str());
        let overlay = overlay_recent(&project, ResponseMode::Facts).await;

        let Some(overlay) = overlay else {
            anyhow::bail!("expected codex overlay");
        };
        let kinds: HashSet<String> = overlay.hits.iter().map(|h| h.kind.clone()).collect();
        assert!(kinds.contains("requirement"), "expected requirement kind");
        assert!(kinds.contains("change"), "expected change kind");
        Ok(())
    }

    #[tokio::test]
    async fn codex_extracts_decision_plan_evidence_sections_and_deprioritizes_commands(
    ) -> anyhow::Result<()> {
        let _permit = lock_codex_home().await;
        let tmp = tempfile::tempdir()?;
        let codex_home = tmp.path().join("codex_home");
        let sessions_root = codex_home.join("sessions/2026/01/06");
        tokio::fs::create_dir_all(&sessions_root).await?;

        let project = tmp.path().join("work/repo");
        tokio::fs::create_dir_all(&project).await?;

        let session = sessions_root
            .join("rollout-2026-01-06T12-02-00-00000000-0000-0000-0000-000000000000.jsonl");

        let cwd = project.to_string_lossy().to_string();
        let session_meta = serde_json::json!({
            "timestamp": "2026-01-06T12:02:00.000Z",
            "type": "session_meta",
            "payload": { "id": "s3", "timestamp": "2026-01-06T12:02:00.000Z", "cwd": cwd }
        });
        let user = serde_json::json!({
            "timestamp": "2026-01-06T12:02:01.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "должно быть без ручной настройки" }]
            }
        });
        let exec_args = serde_json::json!({"cmd": "cd /tmp && ps aux | head -n 5"}).to_string();
        let exec = serde_json::json!({
            "timestamp": "2026-01-06T12:02:01.500Z",
            "type": "response_item",
            "payload": { "type": "function_call", "name": "exec_command", "arguments": exec_args }
        });
        let assistant = serde_json::json!({
            "timestamp": "2026-01-06T12:02:02.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "input_text",
                    "text": "РЕШЕНИЕ: делаем polite governor\nДАЛЬШЕ: проверить через MCP\nДОКАЗАТЕЛЬСТВА: doctor issues=0"
                }]
            }
        });
        let jsonl = format!("{session_meta}\n{user}\n{exec}\n{assistant}\n");
        tokio::fs::write(&session, jsonl).await?;

        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.as_os_str());

        let Some(candidates) =
            load_candidates(&project, ResponseMode::Facts, RefreshKind::Recent).await
        else {
            anyhow::bail!("expected candidates");
        };
        assert!(
            candidates.iter().any(|c| c.kind == "command"),
            "expected command candidate to be extracted"
        );

        let overlay = overlay_recent(&project, ResponseMode::Facts).await;

        let Some(overlay) = overlay else {
            anyhow::bail!("expected codex overlay");
        };
        let kinds: HashSet<String> = overlay.hits.iter().map(|h| h.kind.clone()).collect();
        assert!(kinds.contains("decision"), "expected decision kind");
        assert!(kinds.contains("plan"), "expected plan kind");
        assert!(kinds.contains("evidence"), "expected evidence kind");
        assert!(
            !kinds.contains("command"),
            "expected command traces to be de-prioritized"
        );
        Ok(())
    }

    #[tokio::test]
    async fn codex_extracts_bracketed_decision_plan_evidence_sections() -> anyhow::Result<()> {
        let _permit = lock_codex_home().await;
        let tmp = tempfile::tempdir()?;
        let codex_home = tmp.path().join("codex_home");
        let sessions_root = codex_home.join("sessions/2026/01/06");
        tokio::fs::create_dir_all(&sessions_root).await?;

        let project = tmp.path().join("work/repo");
        tokio::fs::create_dir_all(&project).await?;

        let session = sessions_root
            .join("rollout-2026-01-06T12-02-30-00000000-0000-0000-0000-000000000000.jsonl");

        let cwd = project.to_string_lossy().to_string();
        let session_meta = serde_json::json!({
            "timestamp": "2026-01-06T12:02:30.000Z",
            "type": "session_meta",
            "payload": { "id": "s3b", "timestamp": "2026-01-06T12:02:30.000Z", "cwd": cwd }
        });
        let assistant = serde_json::json!({
            "timestamp": "2026-01-06T12:02:31.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "input_text",
                    "text": "[decision] keep per-session byte cursors\n[plan] ingest only new JSONL lines\n[evidence] read_pack shows external_memory hits"
                }]
            }
        });
        let jsonl = format!("{session_meta}\n{assistant}\n");
        tokio::fs::write(&session, jsonl).await?;

        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.as_os_str());
        let overlay = overlay_recent(&project, ResponseMode::Facts).await;

        let Some(overlay) = overlay else {
            anyhow::bail!("expected codex overlay");
        };
        let kinds: HashSet<String> = overlay.hits.iter().map(|h| h.kind.clone()).collect();
        assert!(kinds.contains("decision"), "expected decision kind");
        assert!(kinds.contains("plan"), "expected plan kind");
        assert!(kinds.contains("evidence"), "expected evidence kind");
        Ok(())
    }

    #[tokio::test]
    async fn codex_extracts_signal_sections_from_update_plan_explanation() -> anyhow::Result<()> {
        let _permit = lock_codex_home().await;
        let tmp = tempfile::tempdir()?;
        let codex_home = tmp.path().join("codex_home");
        let sessions_root = codex_home.join("sessions/2026/01/06");
        tokio::fs::create_dir_all(&sessions_root).await?;

        let project = tmp.path().join("work/repo");
        tokio::fs::create_dir_all(&project).await?;

        let session = sessions_root
            .join("rollout-2026-01-06T12-03-00-00000000-0000-0000-0000-000000000000.jsonl");

        let cwd = project.to_string_lossy().to_string();
        let session_meta = serde_json::json!({
            "timestamp": "2026-01-06T12:03:00.000Z",
            "type": "session_meta",
            "payload": { "id": "s4", "timestamp": "2026-01-06T12:03:00.000Z", "cwd": cwd }
        });
        let update_args = serde_json::json!({
            "explanation": "СДЕЛАЛ: поднял polite governor\nРЕШЕНИЕ: сохраняем качество, но снижаем шум\nДОКАЗАТЕЛЬСТВА: doctor issues=0\nДАЛЬШЕ: прогнать гейты",
            "plan": [
                { "step": "Run quality gates", "status": "in_progress" },
                { "step": "Build+install release", "status": "pending" }
            ]
        })
        .to_string();
        let update_plan = serde_json::json!({
            "timestamp": "2026-01-06T12:03:01.000Z",
            "type": "response_item",
            "payload": { "type": "function_call", "name": "update_plan", "arguments": update_args }
        });
        let jsonl = format!("{session_meta}\n{update_plan}\n");
        tokio::fs::write(&session, jsonl).await?;

        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.as_os_str());
        let overlay = overlay_recent(&project, ResponseMode::Facts).await;

        let Some(overlay) = overlay else {
            anyhow::bail!("expected codex overlay");
        };

        let kinds: HashSet<String> = overlay.hits.iter().map(|h| h.kind.clone()).collect();
        assert!(kinds.contains("plan"), "expected plan from update_plan");
        assert!(
            kinds.contains("decision"),
            "expected decision from explanation"
        );
        assert!(
            kinds.contains("evidence"),
            "expected evidence from explanation"
        );
        Ok(())
    }

    #[tokio::test]
    async fn codex_cache_does_not_forget_plan_hits_when_session_grows() -> anyhow::Result<()> {
        let _permit = lock_codex_home().await;
        let tmp = tempfile::tempdir()?;
        let codex_home = tmp.path().join("codex_home");
        let sessions_root = codex_home.join("sessions");
        let day_root = sessions_root.join("2026/01/06");
        tokio::fs::create_dir_all(&day_root).await?;

        let project = tmp.path().join("work/repo");
        tokio::fs::create_dir_all(&project).await?;

        let session =
            day_root.join("rollout-2026-01-06T12-04-00-00000000-0000-0000-0000-000000000000.jsonl");

        let cwd = project.to_string_lossy().to_string();
        let session_meta = serde_json::json!({
            "timestamp": "2026-01-06T12:04:00.000Z",
            "type": "session_meta",
            "payload": { "id": "s5", "timestamp": "2026-01-06T12:04:00.000Z", "cwd": cwd }
        });
        let update_args = serde_json::json!({
            "plan": [{ "step": "Keep memory stable", "status": "in_progress" }]
        })
        .to_string();
        let update_plan = serde_json::json!({
            "timestamp": "2026-01-06T12:04:01.000Z",
            "type": "response_item",
            "payload": { "type": "function_call", "name": "update_plan", "arguments": update_args }
        });
        let original = format!("{session_meta}\n{update_plan}\n");
        tokio::fs::write(&session, &original).await?;

        let sessions_root_str = sessions_root.to_string_lossy().to_string();
        let cache = CodexSessionsCacheV1 {
            v: CODEX_SESSIONS_CACHE_VERSION,
            built_at_unix_ms: 0,
            sessions_root: sessions_root_str,
            processed_session_mtime_ms: HashMap::new(),
            processed_session_progress: HashMap::new(),
            candidates: Vec::new(),
        };

        let cache = refresh_cache(&project, &sessions_root, cache, ResponseMode::Facts).await;
        assert!(
            cache.candidates.iter().any(|c| c.kind == "plan"),
            "expected initial plan candidate"
        );

        // Append enough noise so the original `update_plan` line falls outside the tail window.
        let mut noise = String::new();
        for i in 0..3200 {
            let line = serde_json::json!({
                "timestamp": "2026-01-06T12:04:02.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": format!("ping {i}") }]
                }
            });
            noise.push_str(&line.to_string());
            noise.push('\n');
        }
        tokio::fs::write(&session, format!("{original}{noise}")).await?;

        let cache = refresh_cache(&project, &sessions_root, cache, ResponseMode::Facts).await;
        assert!(
            cache.candidates.iter().any(|c| c.kind == "plan"),
            "expected plan candidate to remain after session growth"
        );
        Ok(())
    }

    #[tokio::test]
    async fn codex_incremental_cursor_does_not_miss_events_outside_tail_window(
    ) -> anyhow::Result<()> {
        let _permit = lock_codex_home().await;
        let tmp = tempfile::tempdir()?;
        let codex_home = tmp.path().join("codex_home");
        let sessions_root = codex_home.join("sessions");
        let day_root = sessions_root.join("2026/01/06");
        tokio::fs::create_dir_all(&day_root).await?;

        let project = tmp.path().join("work/repo");
        tokio::fs::create_dir_all(&project).await?;

        let session =
            day_root.join("rollout-2026-01-06T12-05-00-00000000-0000-0000-0000-000000000000.jsonl");

        let cwd = project.to_string_lossy().to_string();
        let session_meta = serde_json::json!({
            "timestamp": "2026-01-06T12:05:00.000Z",
            "type": "session_meta",
            "payload": { "id": "s6", "timestamp": "2026-01-06T12:05:00.000Z", "cwd": cwd }
        });
        let original = format!("{session_meta}\n");
        tokio::fs::write(&session, &original).await?;

        let sessions_root_str = sessions_root.to_string_lossy().to_string();
        let cache = CodexSessionsCacheV1 {
            v: CODEX_SESSIONS_CACHE_VERSION,
            built_at_unix_ms: 0,
            sessions_root: sessions_root_str,
            processed_session_mtime_ms: HashMap::new(),
            processed_session_progress: HashMap::new(),
            candidates: Vec::new(),
        };

        let cache = refresh_cache(&project, &sessions_root, cache, ResponseMode::Facts).await;
        let progress = cache
            .processed_session_progress
            .get("s6")
            .expect("expected cursor progress");
        assert!(progress.cursor_bytes > 0, "expected cursor to advance");

        let update_args = serde_json::json!({
            "plan": [{ "step": "Do not lose me", "status": "in_progress" }]
        })
        .to_string();
        let update_plan = serde_json::json!({
            "timestamp": "2026-01-06T12:05:02.000Z",
            "type": "response_item",
            "payload": { "type": "function_call", "name": "update_plan", "arguments": update_args }
        });

        // Append enough trailing noise so the `update_plan` line is far outside the tail window
        // (the old implementation would miss it).
        let mut append = String::new();
        for i in 0..32 {
            let line = serde_json::json!({
                "timestamp": "2026-01-06T12:05:01.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": format!("pre {i}") }]
                }
            });
            append.push_str(&line.to_string());
            append.push('\n');
        }
        append.push_str(&update_plan.to_string());
        append.push('\n');
        for i in 0..5200 {
            let line = serde_json::json!({
                "timestamp": "2026-01-06T12:05:03.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": format!("post {i}") }]
                }
            });
            append.push_str(&line.to_string());
            append.push('\n');
        }
        tokio::fs::write(&session, format!("{original}{append}")).await?;

        let cache = refresh_cache(&project, &sessions_root, cache, ResponseMode::Facts).await;
        assert!(
            cache
                .candidates
                .iter()
                .any(|c| c.session_id == "s6" && c.kind == "plan"),
            "expected plan candidate to be extracted from incremental read"
        );
        Ok(())
    }

    #[tokio::test]
    async fn codex_cursor_does_not_skip_partially_written_jsonl_line() -> anyhow::Result<()> {
        let _permit = lock_codex_home().await;
        let tmp = tempfile::tempdir()?;
        let codex_home = tmp.path().join("codex_home");
        let sessions_root = codex_home.join("sessions");
        let day_root = sessions_root.join("2026/01/06");
        tokio::fs::create_dir_all(&day_root).await?;

        let project = tmp.path().join("work/repo");
        tokio::fs::create_dir_all(&project).await?;

        let session =
            day_root.join("rollout-2026-01-06T12-05-30-00000000-0000-0000-0000-000000000000.jsonl");

        let cwd = project.to_string_lossy().to_string();
        let session_meta = serde_json::json!({
            "timestamp": "2026-01-06T12:05:30.000Z",
            "type": "session_meta",
            "payload": { "id": "s6b", "timestamp": "2026-01-06T12:05:30.000Z", "cwd": cwd }
        });
        let original = format!("{session_meta}\n");
        tokio::fs::write(&session, &original).await?;

        let sessions_root_str = sessions_root.to_string_lossy().to_string();
        let cache = CodexSessionsCacheV1 {
            v: CODEX_SESSIONS_CACHE_VERSION,
            built_at_unix_ms: 0,
            sessions_root: sessions_root_str,
            processed_session_mtime_ms: HashMap::new(),
            processed_session_progress: HashMap::new(),
            candidates: Vec::new(),
        };

        let cache = refresh_cache(&project, &sessions_root, cache, ResponseMode::Facts).await;
        let initial_cursor = cache
            .processed_session_progress
            .get("s6b")
            .expect("expected progress")
            .cursor_bytes;

        // Append a partially written JSON line without a newline terminator. The reader must not
        // advance the cursor past this fragment; otherwise we'd miss the event when the writer
        // finishes it.
        let partial =
            "{\"timestamp\":\"2026-01-06T12:05:31.000Z\",\"type\":\"response_item\",\"payload\":{";
        tokio::fs::write(&session, format!("{original}{partial}")).await?;

        let cache = refresh_cache(&project, &sessions_root, cache, ResponseMode::Facts).await;
        let cursor_after_partial = cache
            .processed_session_progress
            .get("s6b")
            .expect("expected progress")
            .cursor_bytes;
        assert_eq!(
            cursor_after_partial, initial_cursor,
            "cursor should stay at last newline boundary while JSON is incomplete"
        );

        // Finish the JSON line and add a newline; it should now be ingested.
        let rest = "\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"input_text\",\"text\":\"[decision] do not skip partial JSON lines\"}]}}\n";
        tokio::fs::write(&session, format!("{original}{partial}{rest}")).await?;

        let cache = refresh_cache(&project, &sessions_root, cache, ResponseMode::Facts).await;
        assert!(
            cache
                .candidates
                .iter()
                .any(|c| c.session_id == "s6b" && c.kind == "decision"),
            "expected decision candidate to be extracted after the JSON line completes"
        );
        Ok(())
    }

    #[tokio::test]
    async fn codex_merges_duplicate_plan_candidates_by_semantic_key() -> anyhow::Result<()> {
        let _permit = lock_codex_home().await;
        let tmp = tempfile::tempdir()?;
        let codex_home = tmp.path().join("codex_home");
        let sessions_root = codex_home.join("sessions");
        let day_root = sessions_root.join("2026/01/06");
        tokio::fs::create_dir_all(&day_root).await?;

        let project = tmp.path().join("work/repo");
        tokio::fs::create_dir_all(&project).await?;

        let session =
            day_root.join("rollout-2026-01-06T12-06-00-00000000-0000-0000-0000-000000000000.jsonl");

        let cwd = project.to_string_lossy().to_string();
        let session_meta = serde_json::json!({
            "timestamp": "2026-01-06T12:06:00.000Z",
            "type": "session_meta",
            "payload": { "id": "s7", "timestamp": "2026-01-06T12:06:00.000Z", "cwd": cwd }
        });

        let plan1_args = serde_json::json!({
            "plan": [
                { "step": "Add semantic merge", "status": "in_progress" },
                { "step": "Run quality gates", "status": "pending" }
            ]
        })
        .to_string();
        let plan1 = serde_json::json!({
            "timestamp": "2026-01-06T12:06:01.000Z",
            "type": "response_item",
            "payload": { "type": "function_call", "name": "update_plan", "arguments": plan1_args }
        });

        let plan2_args = serde_json::json!({
            "plan": [
                { "step": "Add semantic merge", "status": "completed" },
                { "step": "Run quality gates", "status": "in_progress" }
            ]
        })
        .to_string();
        let plan2 = serde_json::json!({
            "timestamp": "2026-01-06T12:06:02.000Z",
            "type": "response_item",
            "payload": { "type": "function_call", "name": "update_plan", "arguments": plan2_args }
        });

        let jsonl = format!("{session_meta}\n{plan1}\n{plan2}\n");
        tokio::fs::write(&session, jsonl).await?;

        let sessions_root_str = sessions_root.to_string_lossy().to_string();
        let cache = CodexSessionsCacheV1 {
            v: CODEX_SESSIONS_CACHE_VERSION,
            built_at_unix_ms: 0,
            sessions_root: sessions_root_str,
            processed_session_mtime_ms: HashMap::new(),
            processed_session_progress: HashMap::new(),
            candidates: Vec::new(),
        };

        let cache = refresh_cache(&project, &sessions_root, cache, ResponseMode::Facts).await;
        let plan_hits: Vec<&StoredCandidate> = cache
            .candidates
            .iter()
            .filter(|c| c.session_id == "s7" && c.kind == "plan")
            .collect();
        assert_eq!(
            plan_hits.len(),
            1,
            "expected plan candidates to merge by semantic key"
        );
        assert!(
            plan_hits[0]
                .reference
                .as_ref()
                .and_then(|v| v.get("sources"))
                .is_some(),
            "expected merged plan reference to include sources"
        );
        Ok(())
    }

    #[tokio::test]
    async fn codex_merges_duplicate_decisions_by_semantic_key() -> anyhow::Result<()> {
        let _permit = lock_codex_home().await;
        let tmp = tempfile::tempdir()?;
        let codex_home = tmp.path().join("codex_home");
        let sessions_root = codex_home.join("sessions");
        let day_root = sessions_root.join("2026/01/06");
        tokio::fs::create_dir_all(&day_root).await?;

        let project = tmp.path().join("work/repo");
        tokio::fs::create_dir_all(&project).await?;

        let session =
            day_root.join("rollout-2026-01-06T12-07-00-00000000-0000-0000-0000-000000000000.jsonl");

        let cwd = project.to_string_lossy().to_string();
        let session_meta = serde_json::json!({
            "timestamp": "2026-01-06T12:07:00.000Z",
            "type": "session_meta",
            "payload": { "id": "s8", "timestamp": "2026-01-06T12:07:00.000Z", "cwd": cwd }
        });
        let assistant1 = serde_json::json!({
            "timestamp": "2026-01-06T12:07:01.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "input_text", "text": "[decision] Keep per-session byte cursor." }]
            }
        });
        let assistant2 = serde_json::json!({
            "timestamp": "2026-01-06T12:07:02.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "input_text", "text": "DECISION: Keep per session byte cursor" }]
            }
        });
        let jsonl = format!("{session_meta}\n{assistant1}\n{assistant2}\n");
        tokio::fs::write(&session, jsonl).await?;

        let sessions_root_str = sessions_root.to_string_lossy().to_string();
        let cache = CodexSessionsCacheV1 {
            v: CODEX_SESSIONS_CACHE_VERSION,
            built_at_unix_ms: 0,
            sessions_root: sessions_root_str,
            processed_session_mtime_ms: HashMap::new(),
            processed_session_progress: HashMap::new(),
            candidates: Vec::new(),
        };

        let cache = refresh_cache(&project, &sessions_root, cache, ResponseMode::Facts).await;
        let decision_hits: Vec<&StoredCandidate> = cache
            .candidates
            .iter()
            .filter(|c| c.session_id == "s8" && c.kind == "decision")
            .collect();
        assert_eq!(
            decision_hits.len(),
            1,
            "expected decisions to merge by semantic key"
        );
        Ok(())
    }

    #[tokio::test]
    async fn codex_merges_decision_bodies_into_bounded_superset() -> anyhow::Result<()> {
        let _permit = lock_codex_home().await;
        let tmp = tempfile::tempdir()?;
        let codex_home = tmp.path().join("codex_home");
        let sessions_root = codex_home.join("sessions");
        let day_root = sessions_root.join("2026/01/06");
        tokio::fs::create_dir_all(&day_root).await?;

        let project = tmp.path().join("work/repo");
        tokio::fs::create_dir_all(&project).await?;

        let session =
            day_root.join("rollout-2026-01-06T12-08-00-00000000-0000-0000-0000-000000000000.jsonl");

        let cwd = project.to_string_lossy().to_string();
        let session_meta = serde_json::json!({
            "timestamp": "2026-01-06T12:08:00.000Z",
            "type": "session_meta",
            "payload": { "id": "s9", "timestamp": "2026-01-06T12:08:00.000Z", "cwd": cwd }
        });
        let assistant_with_details = serde_json::json!({
            "timestamp": "2026-01-06T12:08:01.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "input_text",
                    "text": "[decision] Keep per-session byte cursor\n- store inode\n- handle truncation"
                }]
            }
        });
        let assistant_summary_only = serde_json::json!({
            "timestamp": "2026-01-06T12:08:02.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "input_text",
                    "text": "[decision] Keep per-session byte cursor"
                }]
            }
        });
        let jsonl = format!("{session_meta}\n{assistant_with_details}\n{assistant_summary_only}\n");
        tokio::fs::write(&session, jsonl).await?;

        let sessions_root_str = sessions_root.to_string_lossy().to_string();
        let cache = CodexSessionsCacheV1 {
            v: CODEX_SESSIONS_CACHE_VERSION,
            built_at_unix_ms: 0,
            sessions_root: sessions_root_str,
            processed_session_mtime_ms: HashMap::new(),
            processed_session_progress: HashMap::new(),
            candidates: Vec::new(),
        };

        let cache = refresh_cache(&project, &sessions_root, cache, ResponseMode::Facts).await;
        let decision_hits: Vec<&StoredCandidate> = cache
            .candidates
            .iter()
            .filter(|c| c.session_id == "s9" && c.kind == "decision")
            .collect();
        assert_eq!(
            decision_hits.len(),
            1,
            "expected decisions to merge into a single candidate"
        );
        assert!(
            decision_hits[0].embed_text.contains("store inode"),
            "expected merged decision to keep details from older message"
        );
        Ok(())
    }
}
