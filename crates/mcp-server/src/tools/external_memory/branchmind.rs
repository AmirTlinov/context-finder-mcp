use crate::tools::schemas::read_pack::ReadPackExternalMemoryResult;
use crate::tools::schemas::response_mode::ResponseMode;
use context_vector_store::context_dir_for_project_root;
use serde_json::Value;
use std::path::Path;

fn candidate_context_pack_paths(root: &Path) -> Vec<(String, std::path::PathBuf)> {
    let preferred = context_dir_for_project_root(root)
        .join("branchmind")
        .join("context_pack.json");

    vec![(
        preferred
            .strip_prefix(root)
            .unwrap_or(&preferred)
            .to_string_lossy()
            .to_string(),
        preferred,
    )]
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

    let mut rel_path: Option<String> = None;
    let mut raw: Option<String> = None;
    for (rel, path) in candidate_context_pack_paths(root) {
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => {
                rel_path = Some(rel);
                raw = Some(text);
                break;
            }
            Err(_) => continue,
        }
    }
    let raw = raw?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;

    // Accept either:
    // - the raw `context_pack.result` object, or
    // - the full BranchMind MCP envelope with a `result` field.
    let pack = parsed.get("result").cloned().unwrap_or(parsed);

    let mut candidates = extract_candidates(&pack, response_mode);
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
        source: "branchmind".to_string(),
        path: rel_path,
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

    let mut rel_path: Option<String> = None;
    let mut raw: Option<String> = None;
    for (rel, path) in candidate_context_pack_paths(root) {
        if let Ok(text) = tokio::fs::read_to_string(&path).await {
            rel_path = Some(rel);
            raw = Some(text);
            break;
        }
    }
    let raw = raw?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    let pack = parsed.get("result").cloned().unwrap_or(parsed);

    let mut candidates = extract_candidates(&pack, response_mode);
    if candidates.is_empty() {
        return None;
    }

    candidates.sort_by(|a, b| {
        super::kind_priority(&b.kind)
            .cmp(&super::kind_priority(&a.kind))
            .then_with(|| b.ts_ms.unwrap_or(0).cmp(&a.ts_ms.unwrap_or(0)))
            .then_with(|| a.kind.cmp(&b.kind))
    });

    let caps = super::diversity_caps(response_mode);
    let mut diversity = super::DiversityState::default();
    let mut hits = Vec::new();
    for candidate in candidates {
        if hits.len() >= super::DEFAULT_MAX_HITS {
            break;
        }
        if !super::allow_candidate_kind(&candidate.kind, &mut diversity, caps) {
            continue;
        }
        let score = 1.0 - (hits.len() as f32 * 0.01);
        hits.push(
            crate::tools::schemas::read_pack::ReadPackExternalMemoryHit {
                kind: candidate.kind,
                title: candidate.title,
                score,
                ts_ms: candidate.ts_ms,
                excerpt: candidate.excerpt,
                reference: candidate.reference,
            },
        );
    }

    if hits.is_empty() {
        return None;
    }

    Some(ReadPackExternalMemoryResult {
        source: "branchmind".to_string(),
        path: rel_path,
        hits,
    })
}

fn extract_candidates(pack: &Value, response_mode: ResponseMode) -> Vec<super::Candidate> {
    let mut out = Vec::new();

    // Signals: highest-signal structured items.
    for key in ["blockers", "decisions", "evidence"] {
        let Some(items) = pack
            .get("signals")
            .and_then(|v| v.get(key))
            .and_then(|v| v.as_array())
        else {
            continue;
        };
        for item in items {
            let kind = item
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or(key)
                .to_string();
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let text = item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let ts_ms = item.get("last_ts_ms").and_then(Value::as_u64);
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let tags = item.get("tags").cloned();

            if title.as_deref().unwrap_or("").trim().is_empty() && text.is_empty() {
                continue;
            }

            let embed_text = super::build_embed_text(&kind, title.as_deref(), &text, 2_048);
            let excerpt = super::trim_to_chars(
                &super::build_embed_text(&kind, title.as_deref(), &text, 1_024),
                super::excerpt_chars(response_mode),
            );
            let reference = Some(serde_json::json!({
                "id": id,
                "tags": tags
            }));

            out.push(super::Candidate {
                kind,
                title,
                ts_ms,
                embed_text,
                excerpt,
                reference,
                lexical_score: 0,
            });
        }
    }

    // Notes + trace: include only note entries with textual content (skip event-only rows).
    for (doc_kind, section_key) in [("notes", "notes"), ("trace", "trace")] {
        let Some(entries) = pack
            .get(section_key)
            .and_then(|v| v.get("entries"))
            .and_then(|v| v.as_array())
        else {
            continue;
        };
        for entry in entries {
            if entry.get("kind").and_then(Value::as_str) != Some("note") {
                continue;
            }
            let title = entry
                .get("title")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let content = entry
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if title.as_deref().unwrap_or("").trim().is_empty() && content.is_empty() {
                continue;
            }

            let kind = if doc_kind == "trace" {
                "trace".to_string()
            } else {
                "note".to_string()
            };
            let ts_ms = entry.get("ts_ms").and_then(Value::as_u64);
            let seq = entry.get("seq").and_then(Value::as_u64);

            let embed_text = super::build_embed_text(&kind, title.as_deref(), &content, 2_048);
            let excerpt = super::trim_to_chars(
                &super::build_embed_text(&kind, title.as_deref(), &content, 1_024),
                super::excerpt_chars(response_mode),
            );
            let reference = Some(serde_json::json!({
                "doc": doc_kind,
                "seq": seq
            }));

            out.push(super::Candidate {
                kind,
                title,
                ts_ms,
                embed_text,
                excerpt,
                reference,
                lexical_score: 0,
            });
        }
    }

    out.truncate(super::MAX_CANDIDATES);
    out
}
