use anyhow::{anyhow, Context as AnyhowContext, Result};
use context_indexer::ToolMeta;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use super::cursor::{cursor_fingerprint, decode_cursor, encode_cursor, CURSOR_VERSION};
use super::meaning_pack::compute_meaning_pack_result;
use super::notebook_store::{
    load_or_init_notebook, notebook_paths_for_scope, resolve_repo_identity, staleness_for_anchor,
};
use super::notebook_types::{AgentRunbook, NotebookScope, RunbookSection};
use super::schemas::meaning_pack::MeaningPackRequest;
use super::schemas::response_mode::ResponseMode;
use super::schemas::runbook_pack::{
    RunbookPackBudget, RunbookPackExpanded, RunbookPackMode, RunbookPackRequest, RunbookPackResult,
    RunbookPackTocItem,
};
use super::schemas::worktree_pack::WorktreePackRequest;
use super::worktree_pack::{compute_worktree_pack_result, render_worktree_pack_block};
use super::{secrets::contains_potential_secret_assignment, util::hex_encode_lower};

const VERSION: u32 = 1;
const DEFAULT_MAX_CHARS: usize = 2_000;
const MIN_MAX_CHARS: usize = 800;
const MAX_MAX_CHARS: usize = 500_000;

#[derive(Debug, Serialize, Deserialize)]
struct RunbookPackCursorV1 {
    v: u32,
    tool: String,
    root_hash: Option<u64>,
    scope: String,
    runbook_id: String,
    section_id: String,
    offset: usize,
    content_hash: u64,
}

fn decode_runbook_pack_cursor(cursor: &str) -> Result<RunbookPackCursorV1> {
    decode_cursor(cursor).with_context(|| "decode runbook_pack cursor")
}

pub(super) async fn compute_runbook_pack_result(
    root: &Path,
    root_display: &str,
    request: &RunbookPackRequest,
    cursor: Option<&str>,
) -> Result<RunbookPackResult> {
    let scope = request.scope.unwrap_or(NotebookScope::Project);
    let max_chars = request
        .max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);

    let identity = resolve_repo_identity(root);
    let paths = notebook_paths_for_scope(root, scope, &identity)?;
    let notebook = load_or_init_notebook(root, &paths)?;
    let runbook = notebook
        .runbooks
        .iter()
        .find(|rb| rb.id == request.runbook_id)
        .cloned()
        .ok_or_else(|| anyhow!("Runbook not found: {}", request.runbook_id))?;

    let mut toc = Vec::new();
    let mut staleness_cache: HashMap<String, (u32, u32)> = HashMap::new();
    for section in &runbook.sections {
        toc.push(compute_toc_item(
            root,
            &notebook.anchors,
            section,
            &mut staleness_cache,
        )?);
    }

    let mut expanded: Option<RunbookPackExpanded> = None;
    let mut mode = request.mode.unwrap_or(RunbookPackMode::Summary);
    let mut section_id = request.section_id.clone();
    let mut section_offset = 0usize;
    let mut content_hash_expected: Option<u64> = None;
    if let Some(cursor) = cursor {
        let decoded = decode_runbook_pack_cursor(cursor)?;
        if decoded.v != CURSOR_VERSION || decoded.tool != "runbook_pack" {
            anyhow::bail!("Invalid cursor: wrong tool/version");
        }
        if let Some(root_hash) = decoded.root_hash {
            let expected = cursor_fingerprint(root_display);
            if root_hash != expected {
                anyhow::bail!("Invalid cursor: different root");
            }
        }
        if decoded.runbook_id != request.runbook_id {
            anyhow::bail!("Invalid cursor: different runbook_id");
        }
        if decoded.scope != scope_to_str(scope) {
            anyhow::bail!("Invalid cursor: different scope");
        }
        mode = RunbookPackMode::Section;
        section_id = Some(decoded.section_id);
        section_offset = decoded.offset;
        content_hash_expected = Some(decoded.content_hash);
    }

    if matches!(mode, RunbookPackMode::Section) {
        let sid = section_id
            .as_deref()
            .ok_or_else(|| anyhow!("mode=section requires section_id"))?;
        let section = runbook
            .sections
            .iter()
            .find(|s| section_id_for(s) == sid)
            .ok_or_else(|| anyhow!("Unknown section_id: {sid}"))?;

        let full_content = render_section_content(
            root,
            root_display,
            response_mode,
            &runbook,
            &notebook,
            section,
            max_chars,
        )
        .await?;
        let content_hash = fingerprint_content(&full_content);
        if let Some(expected) = content_hash_expected {
            if expected != content_hash {
                anyhow::bail!("Invalid cursor: expired continuation");
            }
        }

        let available = max_chars.saturating_sub(200);
        let (slice, used, truncated) = slice_from_offset(&full_content, section_offset, available);
        let next_cursor = if truncated {
            let cursor = RunbookPackCursorV1 {
                v: CURSOR_VERSION,
                tool: "runbook_pack".to_string(),
                root_hash: Some(cursor_fingerprint(root_display)),
                scope: scope_to_str(scope).to_string(),
                runbook_id: request.runbook_id.clone(),
                section_id: sid.to_string(),
                offset: section_offset.saturating_add(used),
                content_hash,
            };
            Some(encode_cursor(&cursor)?)
        } else {
            None
        };

        expanded = Some(RunbookPackExpanded {
            section_id: sid.to_string(),
            content: slice,
            truncated,
            next_cursor,
        });
    }

    let out = RunbookPackResult {
        version: VERSION,
        runbook_id: runbook.id.clone(),
        runbook_title: runbook.title.clone(),
        mode: match mode {
            RunbookPackMode::Summary => "summary".to_string(),
            RunbookPackMode::Section => "section".to_string(),
        },
        toc,
        expanded,
        budget: RunbookPackBudget {
            max_chars,
            used_chars: 0,
            truncated: false,
        },
        next_actions: Vec::new(),
        meta: ToolMeta::default(),
    };
    Ok(out)
}

fn scope_to_str(scope: NotebookScope) -> &'static str {
    match scope {
        NotebookScope::Project => "project",
        NotebookScope::UserRepo => "user_repo",
    }
}

fn section_id_for(section: &RunbookSection) -> &str {
    match section {
        RunbookSection::Anchors { id, .. } => id,
        RunbookSection::MeaningPack { id, .. } => id,
        RunbookSection::Worktrees { id, .. } => id,
    }
}

fn fingerprint_content(content: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let digest = hasher.finalize();
    u64::from_be_bytes([
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
    ])
}

fn slice_from_offset(input: &str, offset: usize, max_len: usize) -> (String, usize, bool) {
    if max_len == 0 {
        return (String::new(), 0, true);
    }
    let total = input.chars().count();
    if offset >= total {
        return (String::new(), 0, false);
    }

    let mut out = String::new();
    let mut used = 0usize;
    for (idx, ch) in input.chars().enumerate() {
        if idx < offset {
            continue;
        }
        if used >= max_len {
            break;
        }
        out.push(ch);
        used += 1;
    }
    let truncated = offset.saturating_add(used) < total;
    (out, used, truncated)
}

fn compute_toc_item(
    root: &Path,
    anchors: &[super::notebook_types::NotebookAnchor],
    section: &RunbookSection,
    staleness_cache: &mut HashMap<String, (u32, u32)>,
) -> Result<RunbookPackTocItem> {
    match section {
        RunbookSection::Anchors {
            id,
            title,
            anchor_ids,
            ..
        } => {
            let mut total_items = 0u32;
            let mut stale_items = 0u32;
            let mut unknown = false;
            for anchor_id in anchor_ids {
                let Some(anchor) = anchors.iter().find(|a| a.id == *anchor_id) else {
                    return Ok(RunbookPackTocItem {
                        id: id.clone(),
                        kind: "anchors".to_string(),
                        title: title.clone(),
                        status: "error".to_string(),
                        total_items: anchor_ids.len() as u32,
                        stale_items: 0,
                    });
                };
                if anchor.evidence.iter().any(|ev| ev.source_hash.is_none()) {
                    unknown = true;
                }
                let key = anchor.id.clone();
                let (t, s) = match staleness_cache.get(&key).copied() {
                    Some(v) => v,
                    None => match staleness_for_anchor(root, anchor) {
                        Ok(v) => {
                            staleness_cache.insert(key.clone(), v);
                            v
                        }
                        Err(_) => {
                            return Ok(RunbookPackTocItem {
                                id: id.clone(),
                                kind: "anchors".to_string(),
                                title: title.clone(),
                                status: "error".to_string(),
                                total_items: anchor_ids.len() as u32,
                                stale_items: 0,
                            });
                        }
                    },
                };
                total_items = total_items.saturating_add(t);
                stale_items = stale_items.saturating_add(s);
            }
            let status = if stale_items > 0 {
                "stale"
            } else if unknown {
                "unknown"
            } else {
                "fresh"
            };
            Ok(RunbookPackTocItem {
                id: id.clone(),
                kind: "anchors".to_string(),
                title: title.clone(),
                status: status.to_string(),
                total_items,
                stale_items,
            })
        }
        RunbookSection::MeaningPack { id, title, .. } => Ok(RunbookPackTocItem {
            id: id.clone(),
            kind: "meaning_pack".to_string(),
            title: title.clone(),
            status: "unknown".to_string(),
            total_items: 0,
            stale_items: 0,
        }),
        RunbookSection::Worktrees { id, title, .. } => Ok(RunbookPackTocItem {
            id: id.clone(),
            kind: "worktrees".to_string(),
            title: title.clone(),
            status: "unknown".to_string(),
            total_items: 0,
            stale_items: 0,
        }),
    }
}

async fn render_section_content(
    root: &Path,
    root_display: &str,
    response_mode: ResponseMode,
    runbook: &AgentRunbook,
    notebook: &super::notebook_types::AgentNotebook,
    section: &RunbookSection,
    request_max_chars: usize,
) -> Result<String> {
    match section {
        RunbookSection::Anchors {
            title,
            anchor_ids,
            include_evidence,
            ..
        } => {
            let mut out = String::new();
            out.push_str(&format!("# {title}\n"));
            let mut hash_cache: HashMap<String, String> = HashMap::new();

            let noise_budget = (runbook.policy.noise_budget.clamp(0.0, 1.0) as f64).max(0.0);
            let mut remaining_noise_chars =
                ((request_max_chars as f64) * noise_budget).round().max(0.0) as usize;
            let min_snippet_chars = 120usize;

            let limit = runbook.policy.max_items_per_section as usize;
            for anchor_id in anchor_ids.iter().take(limit) {
                let Some(anchor) = notebook.anchors.iter().find(|a| a.id == *anchor_id) else {
                    out.push_str(&format!("ANCHOR missing id={anchor_id}\n"));
                    continue;
                };
                let (total, stale) = staleness_for_anchor(root, anchor).unwrap_or((0, 0));
                out.push_str(&format!(
                    "\nANCHOR {} {} (id={}) stale={}/{}\n",
                    format!("{:?}", anchor.kind).to_lowercase(),
                    anchor.label,
                    anchor.id,
                    stale,
                    total
                ));

                if !*include_evidence {
                    for ev in &anchor.evidence {
                        out.push_str(&format!(
                            "  EV {}:{}-{}\n",
                            ev.file, ev.start_line, ev.end_line
                        ));
                    }
                    continue;
                }

                let allow_snippets = remaining_noise_chars >= min_snippet_chars;
                if !allow_snippets {
                    out.push_str("  (evidence content suppressed by runbook noise_budget)\n");
                }

                let max_evidence = 3usize;
                for ev in anchor.evidence.iter().take(max_evidence) {
                    out.push_str(&format!(
                        "  EV {}:{}-{}\n",
                        ev.file, ev.start_line, ev.end_line
                    ));

                    if !allow_snippets {
                        continue;
                    }
                    if remaining_noise_chars < min_snippet_chars {
                        continue;
                    }

                    let rel = ev.file.replace('\\', "/");
                    let expected_hash = ev.source_hash.as_deref().unwrap_or("");
                    let current_hash = if expected_hash.is_empty() {
                        String::new()
                    } else if let Some(v) = hash_cache.get(&rel) {
                        v.clone()
                    } else {
                        let v = compute_file_hash(root, &rel).unwrap_or_default();
                        hash_cache.insert(rel.clone(), v.clone());
                        v
                    };
                    let stale = !expected_hash.is_empty()
                        && !current_hash.is_empty()
                        && current_hash != expected_hash;

                    let max_chars = remaining_noise_chars.min(2_000);
                    let max_lines = 60usize;
                    match read_evidence_window_bounded(
                        root,
                        &rel,
                        ev.start_line,
                        ev.end_line,
                        max_lines,
                        max_chars,
                    ) {
                        Ok((content, truncated)) => {
                            if is_compose_file(&rel)
                                && contains_potential_secret_assignment(&content)
                            {
                                out.push_str(&format!(
                                    "    (refusing to return potential secret snippet; stale={stale})\n"
                                ));
                            } else if content.is_empty() {
                                out.push_str(&format!("    (no content; stale={stale})\n"));
                            } else {
                                out.push_str(&format!("    (stale={stale})\n"));
                                for line in content.lines() {
                                    out.push_str(&format!("    {line}\n"));
                                }
                                if truncated {
                                    out.push_str("    (truncated)\n");
                                }
                                remaining_noise_chars =
                                    remaining_noise_chars.saturating_sub(content.chars().count());
                            }
                        }
                        Err(err) => {
                            out.push_str(&format!("    (error: {err})\n"));
                        }
                    }
                }
            }
            if anchor_ids.len() > limit {
                out.push_str(&format!(
                    "\n(note: {} more anchors suppressed by max_items_per_section)\n",
                    anchor_ids.len().saturating_sub(limit)
                ));
            }
            Ok(out)
        }
        RunbookSection::MeaningPack {
            title,
            query,
            max_chars,
            ..
        } => {
            let mut out = String::new();
            out.push_str(&format!("# {title}\n"));
            let request = MeaningPackRequest {
                path: None,
                query: query.clone(),
                map_depth: None,
                map_limit: None,
                max_chars: max_chars.map(|v| v as usize),
                response_mode: Some(response_mode),
                output_format: None,
                auto_index: Some(true),
                auto_index_budget_ms: Some(15_000),
            };
            let pack = compute_meaning_pack_result(root, root_display, &request).await?;
            out.push_str(&pack.pack);
            Ok(out)
        }
        RunbookSection::Worktrees {
            title,
            max_chars,
            limit,
            ..
        } => {
            let mut out = String::new();
            out.push_str(&format!("# {title}\n"));
            let req = WorktreePackRequest {
                path: None,
                query: None,
                max_chars: max_chars.map(|v| v as usize),
                limit: limit.map(|v| v as usize),
                cursor: None,
                response_mode: Some(ResponseMode::Minimal),
            };
            let wt = compute_worktree_pack_result(root, root_display, &req, None).await?;
            out.push_str(&render_worktree_pack_block(&wt));
            Ok(out)
        }
    }
}

fn is_compose_file(rel: &str) -> bool {
    let file_lc = rel.to_ascii_lowercase();
    file_lc.ends_with("docker-compose.yml")
        || file_lc.ends_with("docker-compose.yaml")
        || file_lc.ends_with("compose.yml")
        || file_lc.ends_with("compose.yaml")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode_lower(&hasher.finalize())
}

fn compute_file_hash(root: &Path, rel: &str) -> Result<String> {
    if super::secrets::is_potential_secret_path(rel) {
        anyhow::bail!("refusing to read potential secret path: {rel}");
    }
    let canonical = root
        .join(Path::new(rel))
        .canonicalize()
        .with_context(|| format!("resolve evidence path '{rel}'"))?;
    if !canonical.starts_with(root) {
        anyhow::bail!("evidence file '{rel}' is outside project root");
    }
    let bytes = std::fs::read(&canonical)
        .with_context(|| format!("read file bytes {}", canonical.display()))?;
    Ok(sha256_hex(&bytes))
}

fn read_evidence_window_bounded(
    root: &Path,
    rel: &str,
    start_line: u32,
    end_line: u32,
    max_lines: usize,
    max_chars: usize,
) -> Result<(String, bool)> {
    if super::secrets::is_potential_secret_path(rel) {
        anyhow::bail!("refusing to read potential secret path: {rel}");
    }
    let canonical = root
        .join(Path::new(rel))
        .canonicalize()
        .with_context(|| format!("resolve evidence path '{rel}'"))?;
    if !canonical.starts_with(root) {
        anyhow::bail!("evidence file '{rel}' is outside project root");
    }

    let start_line = start_line.max(1);
    let end_line = end_line.max(start_line);
    let mut out = String::new();
    let mut truncated = false;

    let file = std::fs::File::open(&canonical)
        .with_context(|| format!("open file {}", canonical.display()))?;
    let reader = BufReader::new(file);
    let mut line_no: u32 = 0;
    let mut used_lines: usize = 0;

    for line in reader.lines() {
        line_no = line_no.saturating_add(1);
        if line_no < start_line {
            continue;
        }
        if line_no > end_line {
            break;
        }
        let line = line.with_context(|| format!("read line {line_no}"))?;
        used_lines = used_lines.saturating_add(1);
        if used_lines > max_lines {
            truncated = true;
            break;
        }
        // Keep the snippet bounded by chars; we prefer to stop early rather than trimming a line.
        let next_len = out.chars().count().saturating_add(line.chars().count() + 1);
        if next_len > max_chars {
            truncated = true;
            break;
        }
        out.push_str(&line);
        out.push('\n');
    }

    Ok((out, truncated))
}
