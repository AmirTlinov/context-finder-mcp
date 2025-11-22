use std::path::Path;

use anyhow::{anyhow, Context, Result};
use globset::{GlobBuilder, GlobMatcher};
use serde::Deserialize;

const BUILTIN_GENERAL: &str = include_str!("../../../profiles/general.json");
const BUILTIN_TARGETED_VENORUS: &str = include_str!("../../../profiles/targeted/venorus.json");

#[derive(Clone, Debug)]
pub struct SearchProfile {
    name: String,
    #[allow(dead_code)]
    description: Option<String>,
    paths: PathRules,
    rerank: RerankConfig,
}

#[derive(Clone, Debug)]
struct PathRules {
    boost: Vec<WeightedMatcher>,
    penalty: Vec<WeightedMatcher>,
    reject: Vec<Matcher>,
    noise: Vec<Matcher>,
    must_hit: Vec<MustHitRule>,
}

#[derive(Clone, Debug)]
struct WeightedMatcher {
    matcher: Matcher,
    weight: f32,
}

#[derive(Clone, Debug)]
struct MustHitRule {
    matcher: Matcher,
    tokens: Vec<String>,
    boost: f32,
}

#[derive(Clone, Debug)]
struct Matcher {
    kind: MatchKind,
    needle: String,
    glob: Option<GlobMatcher>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MatchKind {
    Prefix,
    Suffix,
    Contains,
    Glob,
}

impl Default for MatchKind {
    fn default() -> Self {
        MatchKind::Contains
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawProfile {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    paths: RawPathRules,
    rerank: Option<RawRerankConfig>,
    #[serde(default)]
    must_hit: Vec<RawMustHitRule>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawPathRules {
    #[serde(default)]
    boost: Vec<RawWeightedRule>,
    #[serde(default)]
    penalty: Vec<RawWeightedRule>,
    #[serde(default)]
    reject: Vec<RawRule>,
    #[serde(default)]
    noise: Vec<RawRule>,
}

#[derive(Clone, Debug, Deserialize)]
struct RawWeightedRule {
    pattern: String,
    #[serde(default)]
    kind: MatchKind,
    #[serde(default = "default_weight")]
    weight: f32,
}

impl Default for RawWeightedRule {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            kind: MatchKind::Contains,
            weight: default_weight(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct RawRule {
    pattern: String,
    #[serde(default)]
    kind: MatchKind,
}

impl Default for RawRule {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            kind: MatchKind::Contains,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawMustHitRule {
    pattern: String,
    #[serde(default)]
    kind: MatchKind,
    #[serde(default)]
    tokens: Vec<String>,
    #[serde(default = "default_must_hit_boost")]
    boost: f32,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawRerankConfig {
    thresholds: Option<RawThresholds>,
    bm25: Option<RawBm25>,
    boosts: Option<RawBoosts>,
    must_hit: Option<RawRerankMustHit>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawThresholds {
    min_fuzzy_score: Option<f32>,
    min_semantic_score: Option<f32>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawBm25 {
    k1: Option<f32>,
    b: Option<f32>,
    window: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawBoosts {
    path: Option<f32>,
    symbol: Option<f32>,
    yaml_path: Option<f32>,
    bm25: Option<f32>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawRerankMustHit {
    base_bonus: Option<f32>,
}

#[derive(Clone, Debug)]
pub struct RerankConfig {
    pub thresholds: Thresholds,
    pub bm25: Bm25Config,
    pub boosts: RerankBoosts,
    pub must_hit: RerankMustHit,
}

#[derive(Clone, Debug)]
pub struct Thresholds {
    pub min_fuzzy_score: f32,
    pub min_semantic_score: f32,
}

#[derive(Clone, Debug)]
pub struct Bm25Config {
    pub k1: f32,
    pub b: f32,
    pub window: usize,
}

#[derive(Clone, Debug)]
pub struct RerankBoosts {
    pub path: f32,
    pub symbol: f32,
    pub yaml_path: f32,
    pub bm25: f32,
}

impl Default for RerankBoosts {
    fn default() -> Self {
        Self {
            path: 1.5,
            symbol: 2.0,
            yaml_path: 1.5,
            bm25: 1.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RerankMustHit {
    pub base_bonus: f32,
}

impl Default for RerankMustHit {
    fn default() -> Self {
        Self { base_bonus: 25.0 }
    }
}

impl SearchProfile {
    pub fn builtin(name: &str) -> Option<Self> {
        match name {
            "general" => Some(
                Self::from_bytes("general", BUILTIN_GENERAL.as_bytes(), None)
                    .expect("builtin general profile must parse"),
            ),
            "targeted/venorus" => Self::from_bytes(
                "targeted/venorus",
                BUILTIN_TARGETED_VENORUS.as_bytes(),
                Some("general"),
            )
            .ok(),
            other if other == "venorus" => Self::builtin("targeted/venorus"),
            _ => None,
        }
    }

    pub fn general() -> Self {
        Self::builtin("general").expect("general profile is bundled")
    }

    pub fn from_file(profile_name: &str, path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("Failed to read profile file {}", path.display()))?;
        let base = if profile_name == "general" {
            None
        } else {
            Some("general")
        };
        Self::from_bytes(profile_name, &bytes, base)
    }

    pub fn from_bytes(profile_name: &str, bytes: &[u8], base: Option<&str>) -> Result<Self> {
        let raw = parse_raw(bytes).with_context(|| {
            format!(
                "Profile '{}' is not valid JSON/TOML configuration",
                profile_name
            )
        })?;
        let merged_raw = if let Some(base_name) = base {
            let base_raw = builtin_raw(base_name)?;
            merge_raw_profiles(base_raw, raw)
        } else {
            raw
        };
        Self::from_raw(merged_raw, profile_name)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path_weight(&self, path: &str) -> f32 {
        let lower = path.to_ascii_lowercase();
        let mut weight = 1.0;
        for rule in &self.paths.boost {
            if rule.matcher.matches(&lower) {
                weight *= rule.weight;
            }
        }
        for rule in &self.paths.penalty {
            if rule.matcher.matches(&lower) {
                weight *= rule.weight;
            }
        }
        weight
    }

    pub fn is_rejected(&self, path: &str) -> bool {
        let lower = path.to_ascii_lowercase();
        self.paths.reject.iter().any(|m| m.matches(&lower))
    }

    pub fn is_noise(&self, path: &str) -> bool {
        let lower = path.to_ascii_lowercase();
        self.paths.reject.iter().any(|m| m.matches(&lower))
            || self.paths.noise.iter().any(|m| m.matches(&lower))
    }

    pub fn min_fuzzy_score(&self) -> f32 {
        self.rerank.thresholds.min_fuzzy_score
    }

    pub fn min_semantic_score(&self) -> f32 {
        self.rerank.thresholds.min_semantic_score
    }

    pub fn rerank_config(&self) -> &RerankConfig {
        &self.rerank
    }

    pub fn must_hit_matches(
        &self,
        tokens: &[String],
        chunks: &[context_code_chunker::CodeChunk],
    ) -> Vec<(usize, f32)> {
        let mut hits = Vec::new();
        for rule in &self.paths.must_hit {
            if !rule.tokens.is_empty() && !rule.tokens.iter().all(|t| tokens.contains(t)) {
                continue;
            }
            for (idx, chunk) in chunks.iter().enumerate() {
                let lower = chunk.file_path.to_ascii_lowercase();
                if rule.matcher.matches(&lower) {
                    hits.push((idx, rule.boost.max(1.0)));
                }
            }
        }
        hits
    }

    fn from_raw(raw: RawProfile, fallback_name: &str) -> Result<Self> {
        let name = raw
            .name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| fallback_name.to_string());
        let description = raw.description;
        let paths = PathRules::from_raw(raw.paths, raw.must_hit)?;
        let rerank = RerankConfig::from_raw(raw.rerank)?;

        Ok(Self {
            name,
            description,
            paths,
            rerank,
        })
    }
}

impl PathRules {
    fn from_raw(paths: RawPathRules, must_hit: Vec<RawMustHitRule>) -> Result<Self> {
        Ok(Self {
            boost: build_weighted_matchers(paths.boost)?,
            penalty: build_weighted_matchers(paths.penalty)?,
            reject: build_matchers(paths.reject)?,
            noise: build_matchers(paths.noise)?,
            must_hit: build_must_hit(must_hit)?,
        })
    }
}

impl RerankConfig {
    fn from_raw(raw: Option<RawRerankConfig>) -> Result<Self> {
        let thresholds = merge_thresholds(raw.as_ref().and_then(|r| r.thresholds.clone()));
        let bm25 = merge_bm25(raw.as_ref().and_then(|r| r.bm25.clone()));
        let boosts = merge_boosts(raw.as_ref().and_then(|r| r.boosts.clone()));
        let must_hit = merge_rerank_must_hit(raw.as_ref().and_then(|r| r.must_hit.clone()));
        Ok(Self {
            thresholds,
            bm25,
            boosts,
            must_hit,
        })
    }
}

impl Matcher {
    fn new(kind: MatchKind, needle: String) -> Result<Self> {
        let needle = needle.to_ascii_lowercase();
        let glob = if kind == MatchKind::Glob {
            Some(
                GlobBuilder::new(&needle)
                    .literal_separator(true)
                    .build()
                    .context("Invalid glob pattern")?
                    .compile_matcher(),
            )
        } else {
            None
        };
        Ok(Self { kind, needle, glob })
    }

    fn matches(&self, haystack: &str) -> bool {
        match self.kind {
            MatchKind::Prefix => haystack.starts_with(&self.needle),
            MatchKind::Suffix => haystack.ends_with(&self.needle),
            MatchKind::Contains => haystack.contains(&self.needle),
            MatchKind::Glob => self.glob.as_ref().map_or(false, |g| g.is_match(haystack)),
        }
    }
}

fn build_weighted_matchers(raw: Vec<RawWeightedRule>) -> Result<Vec<WeightedMatcher>> {
    let mut matchers = Vec::with_capacity(raw.len());
    for rule in raw {
        if rule.pattern.trim().is_empty() {
            continue;
        }
        let matcher = Matcher::new(rule.kind, rule.pattern)?;
        matchers.push(WeightedMatcher {
            matcher,
            weight: rule.weight,
        });
    }
    Ok(matchers)
}

fn build_matchers(raw: Vec<RawRule>) -> Result<Vec<Matcher>> {
    let mut matchers = Vec::with_capacity(raw.len());
    for rule in raw {
        if rule.pattern.trim().is_empty() {
            continue;
        }
        matchers.push(Matcher::new(rule.kind, rule.pattern)?);
    }
    Ok(matchers)
}

fn build_must_hit(raw: Vec<RawMustHitRule>) -> Result<Vec<MustHitRule>> {
    let mut rules = Vec::with_capacity(raw.len());
    for rule in raw {
        if rule.pattern.trim().is_empty() {
            continue;
        }
        let matcher = Matcher::new(rule.kind, rule.pattern)?;
        let tokens: Vec<String> = rule
            .tokens
            .into_iter()
            .map(|t| t.to_ascii_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        rules.push(MustHitRule {
            matcher,
            tokens,
            boost: rule.boost,
        });
    }
    Ok(rules)
}

fn builtin_raw(name: &str) -> Result<RawProfile> {
    match name {
        "general" => parse_raw(BUILTIN_GENERAL.as_bytes()),
        "targeted/venorus" | "venorus" => parse_raw(BUILTIN_TARGETED_VENORUS.as_bytes()),
        other => Err(anyhow!("Base profile '{}' not bundled", other)),
    }
}

fn merge_raw_profiles(mut base: RawProfile, overlay: RawProfile) -> RawProfile {
    let mut merged = RawProfile::default();
    merged.name = overlay.name.or(base.name.take());
    merged.description = overlay.description.or(base.description.take());

    merged.paths.boost.extend(base.paths.boost);
    merged.paths.boost.extend(overlay.paths.boost);
    merged.paths.penalty.extend(base.paths.penalty);
    merged.paths.penalty.extend(overlay.paths.penalty);
    merged.paths.reject.extend(base.paths.reject);
    merged.paths.reject.extend(overlay.paths.reject);
    merged.paths.noise.extend(base.paths.noise);
    merged.paths.noise.extend(overlay.paths.noise);

    merged.must_hit.extend(base.must_hit);
    merged.must_hit.extend(overlay.must_hit);

    merged.rerank = match (base.rerank.take(), overlay.rerank) {
        (Some(base_cfg), Some(overlay_cfg)) => Some(merge_rerank(Some(base_cfg), overlay_cfg)),
        (Some(base_cfg), None) => Some(base_cfg),
        (None, Some(overlay_cfg)) => Some(overlay_cfg),
        (None, None) => None,
    };

    merged
}

fn merge_rerank(base: Option<RawRerankConfig>, overlay: RawRerankConfig) -> RawRerankConfig {
    if let Some(mut base_cfg) = base {
        if overlay.thresholds.is_some() {
            base_cfg.thresholds = Some(merge_thresholds_raw(
                base_cfg.thresholds.take(),
                overlay.thresholds,
            ));
        }
        if overlay.bm25.is_some() {
            base_cfg.bm25 = Some(merge_bm25_raw(base_cfg.bm25.take(), overlay.bm25));
        }
        if overlay.boosts.is_some() {
            base_cfg.boosts = Some(merge_boosts_raw(base_cfg.boosts.take(), overlay.boosts));
        }
        if overlay.must_hit.is_some() {
            base_cfg.must_hit = Some(merge_rerank_must_hit_raw(
                base_cfg.must_hit.take(),
                overlay.must_hit,
            ));
        }
        base_cfg
    } else {
        overlay
    }
}

fn merge_thresholds_raw(
    base: Option<RawThresholds>,
    overlay: Option<RawThresholds>,
) -> RawThresholds {
    let base = base.unwrap_or_default();
    let overlay = overlay.unwrap_or_default();
    RawThresholds {
        min_fuzzy_score: overlay.min_fuzzy_score.or(base.min_fuzzy_score),
        min_semantic_score: overlay.min_semantic_score.or(base.min_semantic_score),
    }
}

fn merge_bm25_raw(base: Option<RawBm25>, overlay: Option<RawBm25>) -> RawBm25 {
    let base = base.unwrap_or_default();
    let overlay = overlay.unwrap_or_default();
    RawBm25 {
        k1: overlay.k1.or(base.k1),
        b: overlay.b.or(base.b),
        window: overlay.window.or(base.window),
    }
}

fn merge_boosts_raw(base: Option<RawBoosts>, overlay: Option<RawBoosts>) -> RawBoosts {
    let base = base.unwrap_or_default();
    let overlay = overlay.unwrap_or_default();
    RawBoosts {
        path: overlay.path.or(base.path),
        symbol: overlay.symbol.or(base.symbol),
        yaml_path: overlay.yaml_path.or(base.yaml_path),
        bm25: overlay.bm25.or(base.bm25),
    }
}

fn merge_rerank_must_hit_raw(
    base: Option<RawRerankMustHit>,
    overlay: Option<RawRerankMustHit>,
) -> RawRerankMustHit {
    let base = base.unwrap_or_default();
    let overlay = overlay.unwrap_or_default();
    RawRerankMustHit {
        base_bonus: overlay.base_bonus.or(base.base_bonus),
    }
}

fn merge_thresholds(raw: Option<RawThresholds>) -> Thresholds {
    let raw = raw.unwrap_or_default();
    Thresholds {
        min_fuzzy_score: raw.min_fuzzy_score.unwrap_or(0.15),
        min_semantic_score: raw.min_semantic_score.unwrap_or(0.0),
    }
}

fn merge_bm25(raw: Option<RawBm25>) -> Bm25Config {
    let raw = raw.unwrap_or_default();
    Bm25Config {
        k1: raw.k1.unwrap_or(1.2),
        b: raw.b.unwrap_or(0.75),
        window: raw.window.unwrap_or(180),
    }
}

fn merge_boosts(raw: Option<RawBoosts>) -> RerankBoosts {
    let defaults = RerankBoosts::default();
    let raw = raw.unwrap_or_default();
    RerankBoosts {
        path: raw.path.unwrap_or(defaults.path),
        symbol: raw.symbol.unwrap_or(defaults.symbol),
        yaml_path: raw.yaml_path.unwrap_or(defaults.yaml_path),
        bm25: raw.bm25.unwrap_or(defaults.bm25),
    }
}

fn merge_rerank_must_hit(raw: Option<RawRerankMustHit>) -> RerankMustHit {
    let defaults = RerankMustHit::default();
    let raw = raw.unwrap_or_default();
    RerankMustHit {
        base_bonus: raw.base_bonus.unwrap_or(defaults.base_bonus),
    }
}

fn parse_raw(bytes: &[u8]) -> Result<RawProfile> {
    let as_json = serde_json::from_slice(bytes);
    match as_json {
        Ok(value) => Ok(value),
        Err(json_err) => {
            let utf8 = std::str::from_utf8(bytes).map_err(|err| anyhow!("{json_err}; {err}"))?;
            toml::from_str(utf8).map_err(|toml_err| anyhow!("{json_err}; {toml_err}"))
        }
    }
}

const fn default_weight() -> f32 {
    1.0
}

const fn default_must_hit_boost() -> f32 {
    1.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_code_chunker::{ChunkMetadata, ChunkType, CodeChunk};

    fn chunk(path: &str) -> CodeChunk {
        CodeChunk::new(
            path.to_string(),
            1,
            5,
            "content".to_string(),
            ChunkMetadata::default().chunk_type(ChunkType::Function),
        )
    }

    #[test]
    fn parses_builtin_general() {
        let profile = SearchProfile::builtin("general").unwrap();
        assert_eq!(profile.name(), "general");
        assert!(profile.path_weight("src/lib.rs") > 1.0);
    }

    #[test]
    fn fuzzy_threshold_defaults() {
        let profile = SearchProfile::builtin("general").unwrap();
        assert!(profile.min_fuzzy_score() > 0.0);
    }

    #[test]
    fn rerank_boosts_and_must_hit_from_profile() {
        let profile = SearchProfile::from_bytes(
            "custom",
            br#"{
                "rerank": {
                    "bm25": {"window": 120, "k1": 1.5},
                    "boosts": {"path": 2.0, "symbol": 2.5, "yaml_path": 0.5, "bm25": 1.8},
                    "must_hit": {"base_bonus": 12.0}
                }
            }"#,
            Some("general"),
        )
        .unwrap();

        let rerank = profile.rerank_config();
        assert_eq!(rerank.bm25.window, 120);
        assert!((rerank.boosts.path - 2.0).abs() < f32::EPSILON);
        assert!((rerank.boosts.symbol - 2.5).abs() < f32::EPSILON);
        assert!((rerank.boosts.bm25 - 1.8).abs() < f32::EPSILON);
        assert!((rerank.must_hit.base_bonus - 12.0).abs() < f32::EPSILON);
    }

    #[test]
    fn must_hit_matches_tokens_and_path() {
        let profile = SearchProfile::from_bytes(
            "test",
            br#"
            {
                "must_hit": [
                    {"pattern": "src/core.yaml", "tokens": ["core", "yaml"], "boost": 2.0}
                ]
            }
            "#,
            Some("general"),
        )
        .unwrap();
        let chunks = vec![chunk("src/core.yaml"), chunk("src/lib.rs")];
        let tokens = vec!["core".to_string(), "yaml".to_string()];
        let hits = profile.must_hit_matches(&tokens, &chunks);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, 0);
        assert!(hits[0].1 > 1.0);
    }

    #[test]
    fn matches_glob_rules() {
        let matcher = Matcher::new(MatchKind::Glob, "**/*.rs".to_string()).unwrap();
        assert!(matcher.matches("src/lib.rs"));
        assert!(!matcher.matches("docs/readme.md"));
    }
}
