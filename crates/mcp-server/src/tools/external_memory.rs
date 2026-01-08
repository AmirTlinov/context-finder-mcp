mod branchmind;
mod codex_cli;

use crate::tools::schemas::read_pack::{ReadPackExternalMemoryHit, ReadPackExternalMemoryResult};
use crate::tools::schemas::response_mode::ResponseMode;
use context_vector_store::EmbeddingModel;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

const MAX_CANDIDATES: usize = 200;
const MAX_EMBED_CANDIDATES: usize = 32;
const DEFAULT_MAX_HITS: usize = 3;

#[derive(Clone, Debug)]
struct Candidate {
    kind: String,
    title: Option<String>,
    ts_ms: Option<u64>,
    embed_text: String,
    excerpt: String,
    reference: Option<Value>,
    lexical_score: u32,
}

struct OverlayBudget {
    max_total_hits: usize,
}

fn kind_priority(kind: &str) -> u32 {
    let kind = kind.trim().to_lowercase();
    match kind.as_str() {
        // Highest signal: engineering conclusions first.
        "decision" | "decisions" => 40,
        "plan" => 38,
        "blocker" | "blockers" => 37,
        "evidence" => 36,
        "change" => 34,
        "requirement" | "requirements" => 32,
        // Useful but can be chatty; keep below concrete actions.
        "note" | "trace" => 20,
        // Useful for debugging recall, but too noisy to dominate memory.
        "tool_output" => 18,
        "command" => 15,
        // Low signal: conversational.
        "prompt" => 10,
        "reply" => 5,
        _ => 15,
    }
}

fn budget_for_query(response_mode: ResponseMode) -> OverlayBudget {
    match response_mode {
        ResponseMode::Minimal => OverlayBudget { max_total_hits: 0 },
        ResponseMode::Facts => OverlayBudget { max_total_hits: 5 },
        ResponseMode::Full => OverlayBudget { max_total_hits: 8 },
    }
}

fn budget_for_recent(response_mode: ResponseMode) -> OverlayBudget {
    match response_mode {
        ResponseMode::Minimal => OverlayBudget { max_total_hits: 0 },
        ResponseMode::Facts => OverlayBudget { max_total_hits: 4 },
        ResponseMode::Full => OverlayBudget { max_total_hits: 6 },
    }
}

pub(crate) async fn overlays_for_query(
    root: &std::path::Path,
    query: &str,
    response_mode: ResponseMode,
) -> Vec<ReadPackExternalMemoryResult> {
    let query = query.trim();
    if query.is_empty() || response_mode == ResponseMode::Minimal {
        return Vec::new();
    }

    // Priority order matters for cross-source dedup: keep higher-signal structured sources first.
    let mut results: Vec<ReadPackExternalMemoryResult> = Vec::new();
    if let Some(result) = branchmind::overlay_for_query(root, query, response_mode).await {
        results.push(result);
    }
    if let Some(result) = codex_cli::overlay_for_query(root, query, response_mode).await {
        results.push(result);
    }

    dedup_and_cap_results(&mut results, budget_for_query(response_mode));
    results
}

pub(crate) async fn overlays_recent(
    root: &std::path::Path,
    response_mode: ResponseMode,
) -> Vec<ReadPackExternalMemoryResult> {
    if response_mode == ResponseMode::Minimal {
        return Vec::new();
    }

    let mut results: Vec<ReadPackExternalMemoryResult> = Vec::new();
    if let Some(result) = branchmind::overlay_recent(root, response_mode).await {
        results.push(result);
    }
    if let Some(result) = codex_cli::overlay_recent(root, response_mode).await {
        results.push(result);
    }

    dedup_and_cap_results(&mut results, budget_for_recent(response_mode));
    results
}

fn dedup_and_cap_results(results: &mut Vec<ReadPackExternalMemoryResult>, budget: OverlayBudget) {
    if results.is_empty() || budget.max_total_hits == 0 {
        results.clear();
        return;
    }

    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let mut total_kept = 0usize;

    for result in results.iter_mut() {
        if total_kept >= budget.max_total_hits {
            result.hits.clear();
            continue;
        }

        let mut kept: Vec<ReadPackExternalMemoryHit> = Vec::new();
        for hit in result.hits.drain(..) {
            if total_kept >= budget.max_total_hits {
                break;
            }
            let key = hit_dedup_key(&hit);
            if !seen.insert(key) {
                continue;
            }
            if hit.excerpt.trim().is_empty() {
                continue;
            }
            kept.push(hit);
            total_kept = total_kept.saturating_add(1);
        }
        result.hits = kept;
    }

    results.retain(|r| !r.hits.is_empty());
}

fn hit_dedup_key(hit: &ReadPackExternalMemoryHit) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(hit.kind.as_bytes());
    hasher.update(b"\n");
    if let Some(title) = hit.title.as_deref() {
        hasher.update(title.trim().as_bytes());
    }
    hasher.update(b"\n");
    hasher.update(hit.excerpt.trim().as_bytes());
    hasher.finalize().into()
}

fn build_embed_text(kind: &str, title: Option<&str>, text: &str, max_chars: usize) -> String {
    let title = title.unwrap_or("").trim();
    let text = text.trim();

    let mut out = String::new();
    if !kind.trim().is_empty() {
        out.push('[');
        out.push_str(kind.trim());
        out.push_str("] ");
    }
    if !title.is_empty() {
        out.push_str(title);
    }
    if !text.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(text);
    }

    trim_to_chars(&out, max_chars)
}

fn excerpt_chars(response_mode: ResponseMode) -> usize {
    match response_mode {
        ResponseMode::Minimal => 240,
        ResponseMode::Facts => 420,
        ResponseMode::Full => 800,
    }
}

fn trim_to_chars(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in input.chars().enumerate() {
        if idx >= max_chars {
            break;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

fn apply_lexical_scores(candidates: &mut [Candidate], query: &str) {
    let tokens = query_tokens(query);
    if tokens.is_empty() {
        return;
    }
    for candidate in candidates {
        let hay = candidate.embed_text.to_lowercase();
        let mut score = 0u32;
        for token in &tokens {
            if hay.contains(token) {
                score += 1;
            }
        }
        candidate.lexical_score = score;
    }
}

fn query_tokens(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in query.split_whitespace() {
        let token = raw
            .trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-' && ch != '/')
            .to_lowercase();
        if token.len() < 3 {
            continue;
        }
        out.push(token);
    }
    out.sort();
    out.dedup();
    out.truncate(12);
    out
}

fn select_for_embedding(candidates: &[Candidate]) -> Vec<Candidate> {
    if candidates.is_empty() {
        return Vec::new();
    }

    let mut selected: Vec<Candidate> = Vec::new();
    let mut used = vec![false; candidates.len()];

    // 1) Always reserve capacity for high-signal kinds, even when lexical scoring is dominated by
    // prompts (common for meta queries). This keeps memory overlays useful and reduces noise.
    let mut by_priority: Vec<usize> = (0..candidates.len()).collect();
    by_priority.sort_by(|a, b| {
        kind_priority(&candidates[*b].kind)
            .cmp(&kind_priority(&candidates[*a].kind))
            .then_with(|| {
                candidates[*b]
                    .ts_ms
                    .unwrap_or(0)
                    .cmp(&candidates[*a].ts_ms.unwrap_or(0))
            })
            .then_with(|| candidates[*a].kind.cmp(&candidates[*b].kind))
    });

    let reserve = (MAX_EMBED_CANDIDATES / 2).max(8);
    for idx in by_priority {
        if selected.len() >= reserve {
            break;
        }
        selected.push(candidates[idx].clone());
        used[idx] = true;
    }

    // 2) Fill remaining slots by lexical score (query match), then recency.
    let mut by_lexical: Vec<usize> = (0..candidates.len()).collect();
    by_lexical.sort_by(|a, b| {
        candidates[*b]
            .lexical_score
            .cmp(&candidates[*a].lexical_score)
            .then_with(|| {
                candidates[*b]
                    .ts_ms
                    .unwrap_or(0)
                    .cmp(&candidates[*a].ts_ms.unwrap_or(0))
            })
            .then_with(|| candidates[*a].kind.cmp(&candidates[*b].kind))
    });

    for idx in by_lexical {
        if selected.len() >= MAX_EMBED_CANDIDATES {
            break;
        }
        if used[idx] {
            continue;
        }
        selected.push(candidates[idx].clone());
        used[idx] = true;
    }

    // 3) If we still have no lexical signal at all, fall back to pure recency.
    let has_signal = candidates.iter().any(|c| c.lexical_score > 0);
    if !has_signal {
        let mut recent: Vec<usize> = (0..candidates.len()).collect();
        recent.sort_by(|a, b| {
            candidates[*b]
                .ts_ms
                .unwrap_or(0)
                .cmp(&candidates[*a].ts_ms.unwrap_or(0))
        });
        for idx in recent {
            if selected.len() >= MAX_EMBED_CANDIDATES {
                break;
            }
            if used[idx] {
                continue;
            }
            selected.push(candidates[idx].clone());
            used[idx] = true;
        }
    }

    selected
}

#[derive(Clone, Copy, Debug, Default)]
struct DiversityState {
    prompts: usize,
    replies: usize,
}

#[derive(Clone, Copy, Debug)]
struct DiversityCaps {
    max_prompts: usize,
    max_replies: usize,
}

fn diversity_caps(response_mode: ResponseMode) -> DiversityCaps {
    match response_mode {
        ResponseMode::Minimal => DiversityCaps {
            max_prompts: 0,
            max_replies: 0,
        },
        ResponseMode::Facts => DiversityCaps {
            max_prompts: 1,
            max_replies: 0,
        },
        ResponseMode::Full => DiversityCaps {
            max_prompts: 2,
            max_replies: 1,
        },
    }
}

fn allow_candidate_kind(kind: &str, state: &mut DiversityState, caps: DiversityCaps) -> bool {
    match kind.trim().to_lowercase().as_str() {
        "prompt" => {
            if state.prompts >= caps.max_prompts {
                return false;
            }
            state.prompts = state.prompts.saturating_add(1);
            true
        }
        "reply" => {
            if state.replies >= caps.max_replies {
                return false;
            }
            state.replies = state.replies.saturating_add(1);
            true
        }
        _ => true,
    }
}

fn candidate_to_hit(candidate: Candidate, score: f32) -> ReadPackExternalMemoryHit {
    ReadPackExternalMemoryHit {
        kind: candidate.kind,
        title: candidate.title,
        score,
        ts_ms: candidate.ts_ms,
        excerpt: candidate.excerpt,
        reference: candidate.reference,
    }
}

async fn rank_candidates(
    query: &str,
    candidates: Vec<Candidate>,
    response_mode: ResponseMode,
) -> Vec<ReadPackExternalMemoryHit> {
    if candidates.is_empty() {
        return Vec::new();
    }

    let caps = diversity_caps(response_mode);

    // Facts mode is the default “daily driver” for most agents. Keep it low-noise and cheap:
    // do not eagerly initialize the embedding backend just to rank memory overlays.
    if response_mode != ResponseMode::Full {
        let mut matched: Vec<Candidate> = candidates
            .iter()
            .filter(|c| c.lexical_score > 0)
            .cloned()
            .collect();
        matched.sort_by(|a, b| {
            kind_priority(&b.kind)
                .cmp(&kind_priority(&a.kind))
                .then_with(|| b.lexical_score.cmp(&a.lexical_score))
                .then_with(|| b.ts_ms.unwrap_or(0).cmp(&a.ts_ms.unwrap_or(0)))
                .then_with(|| a.kind.cmp(&b.kind))
        });

        let mut hits: Vec<ReadPackExternalMemoryHit> = Vec::new();
        let mut diversity = DiversityState::default();

        for candidate in matched {
            if hits.len() >= DEFAULT_MAX_HITS {
                break;
            }
            if !allow_candidate_kind(&candidate.kind, &mut diversity, caps) {
                continue;
            }
            let score = 1.0 - (hits.len() as f32 * 0.01);
            hits.push(candidate_to_hit(candidate, score));
        }

        if hits.len() >= DEFAULT_MAX_HITS {
            return hits;
        }

        // Fill remaining slots with high-signal recent items to avoid returning only prompts.
        let mut fallback: Vec<Candidate> = candidates;
        fallback.sort_by(|a, b| {
            kind_priority(&b.kind)
                .cmp(&kind_priority(&a.kind))
                .then_with(|| b.ts_ms.unwrap_or(0).cmp(&a.ts_ms.unwrap_or(0)))
                .then_with(|| b.lexical_score.cmp(&a.lexical_score))
                .then_with(|| a.kind.cmp(&b.kind))
        });

        for candidate in fallback {
            if hits.len() >= DEFAULT_MAX_HITS {
                break;
            }
            if !allow_candidate_kind(&candidate.kind, &mut diversity, caps) {
                continue;
            }
            let score = 1.0 - (hits.len() as f32 * 0.01);
            hits.push(candidate_to_hit(candidate, score));
        }
        return hits;
    }

    let model_id = context_vector_store::current_model_id().unwrap_or_else(|_| "bge-small".into());
    let embedder = EmbeddingModel::new_for_model(&model_id);

    // If embeddings are unavailable (e.g., GPU not configured), fall back to lexical ranking.
    let Ok(embedder) = embedder else {
        let mut candidates = candidates;
        candidates.sort_by(|a, b| {
            kind_priority(&b.kind)
                .cmp(&kind_priority(&a.kind))
                .then_with(|| b.lexical_score.cmp(&a.lexical_score))
                .then_with(|| b.ts_ms.unwrap_or(0).cmp(&a.ts_ms.unwrap_or(0)))
                .then_with(|| a.kind.cmp(&b.kind))
        });

        let mut hits: Vec<ReadPackExternalMemoryHit> = Vec::new();
        let mut diversity = DiversityState::default();
        for candidate in candidates {
            if hits.len() >= DEFAULT_MAX_HITS {
                break;
            }
            if !allow_candidate_kind(&candidate.kind, &mut diversity, caps) {
                continue;
            }
            let score = 1.0 - (hits.len() as f32 * 0.01);
            hits.push(candidate_to_hit(candidate, score));
        }
        return hits;
    };

    let Ok(query_vec) = embedder.embed(query).await else {
        return Vec::new();
    };

    let texts: Vec<&str> = candidates.iter().map(|c| c.embed_text.as_str()).collect();
    let Ok(vectors) = embedder.embed_batch(texts).await else {
        return Vec::new();
    };

    let mut scored: Vec<(f32, Candidate)> = vectors
        .into_iter()
        .zip(candidates.into_iter())
        .map(|(vec, cand)| {
            let sim = EmbeddingModel::cosine_similarity(&query_vec, &vec);
            // Tiny lexical nudge for deterministic tie-breaks under stub embeddings.
            let nudged = sim + (cand.lexical_score as f32 * 0.005);
            (nudged, cand)
        })
        .collect();

    scored.sort_by(|(a_score, a), (b_score, b)| {
        b_score
            .total_cmp(a_score)
            .then_with(|| b.ts_ms.unwrap_or(0).cmp(&a.ts_ms.unwrap_or(0)))
            .then_with(|| kind_priority(&b.kind).cmp(&kind_priority(&a.kind)))
            .then_with(|| a.kind.cmp(&b.kind))
    });

    let mut hits: Vec<ReadPackExternalMemoryHit> = Vec::new();
    let mut diversity = DiversityState::default();
    for (score, candidate) in scored {
        if hits.len() >= DEFAULT_MAX_HITS {
            break;
        }
        if !allow_candidate_kind(&candidate.kind, &mut diversity, caps) {
            continue;
        }
        hits.push(candidate_to_hit(candidate, score));
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_embed_text_is_bounded_and_dense() {
        let out = build_embed_text("decision", Some("Title"), "Body", 64);
        assert!(out.contains("[decision]"));
        assert!(out.contains("Title"));
        assert!(out.contains("Body"));
        assert!(out.chars().count() <= 64);
    }
}
