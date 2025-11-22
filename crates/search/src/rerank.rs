use crate::profile::{Bm25Config, RerankBoosts, RerankConfig, SearchProfile};
use context_code_chunker::CodeChunk;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub(crate) struct CandidateSignal {
    pub idx: usize,
    pub fused: f32,
    pub semantic: Option<f32>,
    pub fuzzy: Option<f32>,
}

pub(crate) fn rerank_candidates(
    profile: &SearchProfile,
    chunks: &[CodeChunk],
    tokens: &[String],
    fused_scores: Vec<(usize, f32)>,
    semantic_scores: &HashMap<usize, f32>,
    fuzzy_scores: &HashMap<usize, f32>,
) -> Vec<(usize, f32)> {
    if fused_scores.is_empty() {
        return Vec::new();
    }

    let rerank_cfg = profile.rerank_config().clone();
    let candidates = attach_signals(fused_scores, semantic_scores, fuzzy_scores);
    let filtered = filter_candidates(profile, chunks, &rerank_cfg, candidates);
    if filtered.is_empty() {
        return Vec::new();
    }

    let bm25 = Bm25Context::build(
        rerank_cfg.bm25.clone(),
        chunks,
        &filtered,
        tokens,
        rerank_cfg.boosts.bm25,
    );

    let mut reranked = Vec::with_capacity(filtered.len());
    for candidate in filtered {
        let Some(chunk) = chunks.get(candidate.idx) else {
            continue;
        };

        let mut score = candidate.fused + bm25.score(candidate.idx, tokens);
        score += symbol_bonus(chunk, tokens, &rerank_cfg.boosts);
        score += path_bonus(chunk, tokens, &rerank_cfg.boosts);

        reranked.push((candidate.idx, score));
    }

    reranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
    reranked.dedup_by(|a, b| a.0 == b.0);

    inject_must_hits(
        profile,
        chunks,
        tokens,
        &mut reranked,
        rerank_cfg.must_hit.base_bonus,
    );

    reranked
}

fn attach_signals(
    fused_scores: Vec<(usize, f32)>,
    semantic_scores: &HashMap<usize, f32>,
    fuzzy_scores: &HashMap<usize, f32>,
) -> Vec<CandidateSignal> {
    fused_scores
        .into_iter()
        .map(|(idx, fused)| CandidateSignal {
            idx,
            fused,
            semantic: semantic_scores
                .get(&idx)
                .copied()
                .filter(|s| s.is_finite()),
            fuzzy: fuzzy_scores
                .get(&idx)
                .copied()
                .filter(|s| s.is_finite()),
        })
        .collect()
}

fn filter_candidates(
    profile: &SearchProfile,
    chunks: &[CodeChunk],
    cfg: &RerankConfig,
    candidates: Vec<CandidateSignal>,
) -> Vec<CandidateSignal> {
    candidates
        .into_iter()
        .filter(|candidate| {
            let Some(chunk) = chunks.get(candidate.idx) else {
                return false;
            };
            if profile.is_noise(&chunk.file_path) {
                return false;
            }
            candidate.passes_thresholds(cfg)
        })
        .collect()
}

impl CandidateSignal {
    fn passes_thresholds(&self, cfg: &RerankConfig) -> bool {
        let meets_semantic = self
            .semantic
            .map(|s| s >= cfg.thresholds.min_semantic_score);
        let meets_fuzzy = self.fuzzy.map(|s| s >= cfg.thresholds.min_fuzzy_score);

        match (meets_semantic, meets_fuzzy) {
            (Some(true), _) | (_, Some(true)) => true,
            (Some(false), Some(false)) => false,
            (Some(false), None) | (None, Some(false)) => false,
            (None, None) => true,
        }
    }
}

struct Bm25Context {
    cfg: Bm25Config,
    docs: HashMap<usize, Vec<String>>,
    doc_freq: HashMap<String, usize>,
    avg_len: f32,
    weight: f32,
}

impl Bm25Context {
    fn build(
        cfg: Bm25Config,
        chunks: &[CodeChunk],
        candidates: &[CandidateSignal],
        query_tokens: &[String],
        weight: f32,
    ) -> Self {
        let mut docs = HashMap::new();
        let mut doc_freq = HashMap::new();
        let mut total_len = 0usize;
        let query_terms: HashSet<String> = query_tokens.iter().cloned().collect();

        for candidate in candidates {
            let Some(chunk) = chunks.get(candidate.idx) else {
                continue;
            };
            let tokens = tokenize_content(&chunk.content, cfg.window, &query_terms);
            if tokens.is_empty() {
                continue;
            }
            total_len += tokens.len();
            let mut seen = HashSet::new();
            for token in &tokens {
                if seen.insert(token.as_str()) {
                    *doc_freq.entry(token.clone()).or_insert(0) += 1;
                }
            }
            docs.insert(candidate.idx, tokens);
        }

        let doc_count = docs.len().max(1);
        let avg_len = (total_len as f32) / doc_count as f32;

        Self {
            cfg,
            docs,
            doc_freq,
            avg_len,
            weight,
        }
    }

    fn score(&self, idx: usize, query_tokens: &[String]) -> f32 {
        let Some(doc_tokens) = self.docs.get(&idx) else {
            return 0.0;
        };
        if doc_tokens.is_empty() {
            return 0.0;
        }

        let dl = doc_tokens.len() as f32;
        let total_docs = self.docs.len().max(1) as f32;
        let mut score = 0.0;

        for token in query_tokens {
            let freq = term_frequency(doc_tokens, token);
            if freq <= 0.0 {
                continue;
            }
            let df = *self.doc_freq.get(token).unwrap_or(&0) as f32;
            let idf = bm25_idf(total_docs, df);
            let denom = freq
                + self.cfg.k1
                    * (1.0 - self.cfg.b + self.cfg.b * dl / self.avg_len.max(1e-3));
            if denom > 0.0 {
                score += idf * (freq * (self.cfg.k1 + 1.0)) / denom;
            }
        }

        score * self.weight
    }
}

fn tokenize_content(
    content: &str,
    window: usize,
    allow_list: &HashSet<String>,
) -> Vec<String> {
    if window == 0 || allow_list.is_empty() {
        return Vec::new();
    }

    let mut tokens = Vec::new();
    for part in content.split(|c: char| !c.is_ascii_alphanumeric()) {
        if tokens.len() >= window {
            break;
        }
        let normalized = part.to_ascii_lowercase();
        if normalized.len() < 3 {
            continue;
        }
        if !allow_list.contains(&normalized) {
            continue;
        }
        tokens.push(normalized);
    }
    tokens
}

fn term_frequency(doc_tokens: &[String], needle: &str) -> f32 {
    doc_tokens
        .iter()
        .filter(|token| token.as_str() == needle)
        .count() as f32
}

fn bm25_idf(total_docs: f32, df: f32) -> f32 {
    ((total_docs - df + 0.5) / (df + 0.5) + 1.0).ln()
}

fn path_bonus(chunk: &CodeChunk, tokens: &[String], boosts: &RerankBoosts) -> f32 {
    if tokens.is_empty() {
        return 0.0;
    }
    let path = chunk.file_path.to_ascii_lowercase();
    let mut bonus = 0.0;
    if tokens.iter().any(|token| path.contains(token)) {
        bonus += boosts.path;
        if is_yaml_path(&path) {
            bonus += boosts.yaml_path;
        }
    }
    bonus
}

fn symbol_bonus(chunk: &CodeChunk, tokens: &[String], boosts: &RerankBoosts) -> f32 {
    let Some(symbol) = chunk.metadata.symbol_name.as_ref() else {
        return 0.0;
    };
    let symbol = symbol.to_ascii_lowercase();
    tokens
        .iter()
        .any(|token| symbol.contains(token))
        .then_some(boosts.symbol)
        .unwrap_or(0.0)
}

fn is_yaml_path(path: &str) -> bool {
    path.ends_with(".yaml") || path.ends_with(".yml")
}

fn inject_must_hits(
    profile: &SearchProfile,
    chunks: &[CodeChunk],
    tokens: &[String],
    reranked: &mut Vec<(usize, f32)>,
    base_bonus: f32,
) {
    let base = reranked.first().map(|(_, score)| *score).unwrap_or(0.0);
    let target = base + base_bonus.max(0.0);
    for (idx, boost) in profile.must_hit_matches(tokens, chunks) {
        if let Some((_, score)) = reranked.iter_mut().find(|(existing, _)| *existing == idx) {
            *score = score.max(target * boost.max(1.0));
        } else {
            reranked.push((idx, target * boost.max(1.0)));
        }
    }
    reranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
    reranked.dedup_by(|a, b| a.0 == b.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hybrid::query_tokens;
    use context_code_chunker::{ChunkMetadata, ChunkType};

    fn chunk(path: &str, symbol: &str, content: &str) -> CodeChunk {
        CodeChunk::new(
            path.to_string(),
            1,
            10,
            content.to_string(),
            ChunkMetadata::default()
                .chunk_type(ChunkType::Function)
                .symbol_name(symbol),
        )
    }

    fn map_scores(items: &[(usize, f32)]) -> HashMap<usize, f32> {
        items.iter().copied().collect()
    }

    #[test]
    fn prunes_candidates_below_thresholds() {
        let profile = SearchProfile::from_bytes(
            "test",
            br#"{
                "rerank": {"thresholds": {"min_fuzzy_score": 0.2, "min_semantic_score": 0.4}}
            }"#,
            Some("general"),
        )
        .unwrap();
        let chunks = vec![chunk("src/a.rs", "a", "alpha beta"), chunk("src/b.rs", "b", "beta")];
        let tokens = query_tokens("alpha beta");
        let fused = vec![(0, 1.0), (1, 0.8)];
        let semantic = map_scores(&[(0, 0.1), (1, 0.6)]);
        let fuzzy = map_scores(&[(0, 0.1), (1, 0.9)]);

        let reranked =
            rerank_candidates(&profile, &chunks, &tokens, fused, &semantic, &fuzzy);

        assert_eq!(reranked.len(), 1);
        assert_eq!(reranked[0].0, 1);
    }

    #[test]
    fn path_and_symbol_matches_are_prioritized() {
        let profile = SearchProfile::from_bytes(
            "test",
            br#"{
                "rerank": {
                    "boosts": {"path": 1.8, "symbol": 2.5, "yaml_path": 0.5}
                }
            }"#,
            Some("general"),
        )
        .unwrap();
        let chunks = vec![
            chunk("src/api/config.yaml", "load_config", "load configuration"),
            chunk("src/lib.rs", "lib_entry", "core lib"),
        ];
        let tokens = query_tokens("config api");
        let fused = vec![(0, 0.5), (1, 0.5)];
        let semantic = map_scores(&[(0, 0.9), (1, 0.9)]);
        let fuzzy = map_scores(&[(0, 0.3), (1, 0.3)]);

        let reranked =
            rerank_candidates(&profile, &chunks, &tokens, fused, &semantic, &fuzzy);

        assert_eq!(reranked[0].0, 0);
        assert!(reranked[0].1 > reranked[1].1);
    }

    #[test]
    fn bm25_scoring_uses_window() {
        let profile = SearchProfile::from_bytes(
            "test",
            br#"{
                "rerank": {
                    "bm25": {"window": 5},
                    "boosts": {"bm25": 2.0}
                }
            }"#,
            Some("general"),
        )
        .unwrap();
        let chunks = vec![
            chunk(
                "src/a.rs",
                "compute_window",
                "window window window logic extra tokens beyond window",
            ),
            chunk("src/b.rs", "other", "completely unrelated content"),
        ];
        let tokens = query_tokens("window logic");
        let fused = vec![(0, 0.5), (1, 0.5)];
        let semantic = map_scores(&[(0, 0.8), (1, 0.8)]);
        let fuzzy = map_scores(&[(0, 0.8), (1, 0.8)]);

        let reranked =
            rerank_candidates(&profile, &chunks, &tokens, fused, &semantic, &fuzzy);

        assert_eq!(reranked[0].0, 0);
        assert!(reranked[0].1 > reranked[1].1);
    }

    #[test]
    fn must_hits_are_injected_with_configured_bonus() {
        let profile = SearchProfile::from_bytes(
            "test",
            br#"{
                "must_hit": [
                    {"pattern": "configs/target.yaml", "tokens": ["target"], "boost": 2.0}
                ],
                "rerank": {
                    "must_hit": {"base_bonus": 10.0}
                }
            }"#,
            Some("general"),
        )
        .unwrap();
        let chunks = vec![
            chunk("configs/target.yaml", "root", "target config"),
            chunk("src/lib.rs", "lib", "library code"),
        ];
        let tokens = query_tokens("target");
        let fused = vec![(1, 1.0)];
        let semantic = map_scores(&[(1, 0.9)]);
        let fuzzy = map_scores(&[(1, 0.9)]);

        let reranked =
            rerank_candidates(&profile, &chunks, &tokens, fused, &semantic, &fuzzy);

        assert_eq!(reranked[0].0, 0);
        assert!(reranked[0].1 >= 11.0);
    }
}
