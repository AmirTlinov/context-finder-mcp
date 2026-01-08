use std::path::Path;

use anyhow::{anyhow, Context, Result};
use context_vector_store::{EmbeddingTemplates, QueryKind};
use globset::{GlobBuilder, GlobMatcher};
use serde::Deserialize;

const BUILTIN_GENERAL: &str = include_str!("../../../profiles/general.json");
const BUILTIN_FAST: &str = include_str!("../../../profiles/fast.json");
const BUILTIN_QUALITY: &str = include_str!("../../../profiles/quality.json");
const BUILTIN_TARGETED_VENORUS: &str = include_str!("../../../profiles/targeted/venorus.json");

#[derive(Clone, Debug)]
pub struct SearchProfile {
    name: String,
    #[allow(dead_code)]
    description: Option<String>,
    paths: PathRules,
    rerank: RerankConfig,
    graph_nodes: GraphNodesConfig,
    embedding: EmbeddingTemplates,
    experts: ExpertsConfig,
}

#[derive(Clone, Debug)]
pub struct ExpertsConfig {
    semantic: SemanticExpertsConfig,
    graph_nodes: Vec<String>,
}

#[derive(Clone, Debug)]
struct SemanticExpertsConfig {
    default: Vec<String>,
    identifier: Vec<String>,
    path: Vec<String>,
    conceptual: Vec<String>,
}

impl Default for SemanticExpertsConfig {
    fn default() -> Self {
        let default = vec!["bge-small".to_string()];
        Self {
            default: default.clone(),
            identifier: default.clone(),
            path: default.clone(),
            conceptual: default,
        }
    }
}

impl Default for ExpertsConfig {
    fn default() -> Self {
        let semantic = SemanticExpertsConfig::default();
        let graph_nodes = semantic.default.clone();
        Self {
            semantic,
            graph_nodes,
        }
    }
}

impl ExpertsConfig {
    fn from_raw(raw: Option<RawExpertsConfig>) -> Result<Self> {
        let mut cfg = Self::default();
        let raw = raw.unwrap_or_default();

        if let Some(schema_version) = raw.schema_version {
            if schema_version != 1 {
                return Err(anyhow!(
                    "experts.schema_version {schema_version} is not supported (expected 1)"
                ));
            }
        }

        if let Some(semantic) = raw.semantic {
            let default = semantic
                .default
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| cfg.semantic.default.clone());
            cfg.semantic.default = default;
            let default = cfg.semantic.default.clone();
            cfg.semantic.identifier = semantic
                .identifier
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.clone());
            cfg.semantic.path = semantic
                .path
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.clone());
            cfg.semantic.conceptual = semantic
                .conceptual
                .filter(|v| !v.is_empty())
                .unwrap_or(default);
        }

        if let Some(graph_nodes) = raw.graph_nodes {
            cfg.graph_nodes = graph_nodes
                .default
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| cfg.semantic.default.clone());
        }

        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        validate_model_list("experts.semantic.default", &self.semantic.default)?;
        validate_model_list("experts.semantic.identifier", &self.semantic.identifier)?;
        validate_model_list("experts.semantic.path", &self.semantic.path)?;
        validate_model_list("experts.semantic.conceptual", &self.semantic.conceptual)?;
        validate_model_list("experts.graph_nodes.default", &self.graph_nodes)?;

        Ok(())
    }

    #[must_use]
    pub fn semantic_models(&self, kind: QueryKind) -> &[String] {
        match kind {
            QueryKind::Identifier => &self.semantic.identifier,
            QueryKind::Path => &self.semantic.path,
            QueryKind::Conceptual => &self.semantic.conceptual,
        }
    }

    #[must_use]
    pub fn graph_node_models(&self) -> &[String] {
        &self.graph_nodes
    }
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
#[derive(Default)]
pub enum MatchKind {
    Prefix,
    Suffix,
    #[default]
    Contains,
    Glob,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawProfile {
    #[serde(default)]
    schema_version: Option<u32>,
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    paths: RawPathRules,
    rerank: Option<RawRerankConfig>,
    #[serde(default)]
    must_hit: Vec<RawMustHitRule>,
    #[serde(default)]
    graph_nodes: Option<RawGraphNodesConfig>,
    #[serde(default)]
    embedding: Option<RawEmbeddingConfig>,
    #[serde(default)]
    experts: Option<RawExpertsConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawExpertsConfig {
    schema_version: Option<u32>,
    #[serde(default)]
    semantic: Option<RawSemanticExpertsConfig>,
    #[serde(default)]
    graph_nodes: Option<RawGraphNodeExpertsConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawSemanticExpertsConfig {
    default: Option<Vec<String>>,
    identifier: Option<Vec<String>>,
    path: Option<Vec<String>>,
    conceptual: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawGraphNodeExpertsConfig {
    default: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawEmbeddingConfig {
    schema_version: Option<u32>,
    max_chars: Option<usize>,
    query: Option<RawQueryTemplates>,
    document: Option<RawDocumentTemplates>,
    graph_node: Option<RawGraphNodeTemplates>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawQueryTemplates {
    default: Option<String>,
    identifier: Option<String>,
    path: Option<String>,
    conceptual: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawDocumentTemplates {
    default: Option<String>,
    code: Option<String>,
    docs: Option<String>,
    config: Option<String>,
    test: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawGraphNodeTemplates {
    default: Option<String>,
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

#[derive(Clone, Debug)]
pub struct GraphNodesConfig {
    pub enabled: bool,
    pub weight: f32,
    pub top_k: usize,
    pub max_neighbors_per_relation: usize,
}

impl Default for GraphNodesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            weight: 0.25,
            top_k: 25,
            max_neighbors_per_relation: 12,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct RawGraphNodesConfig {
    enabled: Option<bool>,
    weight: Option<f32>,
    top_k: Option<usize>,
    max_neighbors_per_relation: Option<usize>,
}

impl SearchProfile {
    #[must_use]
    pub fn builtin(name: &str) -> Option<Self> {
        match name {
            "general" => Some(
                Self::from_bytes("general", BUILTIN_GENERAL.as_bytes(), None)
                    .expect("builtin general profile must parse"),
            ),
            "fast" => Self::from_bytes("fast", BUILTIN_FAST.as_bytes(), Some("general")).ok(),
            "quality" => {
                Self::from_bytes("quality", BUILTIN_QUALITY.as_bytes(), Some("general")).ok()
            }
            "targeted/venorus" => Self::from_bytes(
                "targeted/venorus",
                BUILTIN_TARGETED_VENORUS.as_bytes(),
                Some("general"),
            )
            .ok(),
            "venorus" => Self::builtin("targeted/venorus"),
            _ => None,
        }
    }

    #[must_use]
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
            format!("Profile '{profile_name}' is not valid JSON/TOML configuration")
        })?;
        let merged_raw = if let Some(base_name) = base {
            let base_raw = builtin_raw(base_name)?;
            merge_raw_profiles(base_raw, raw)
        } else {
            raw
        };
        Self::from_raw(merged_raw, profile_name)
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn path_boost_weight(&self, path: &str) -> f32 {
        let lower = path.to_ascii_lowercase();
        let mut weight = 1.0;
        for rule in &self.paths.boost {
            if rule.matcher.matches(&lower) {
                weight *= rule.weight;
            }
        }
        weight
    }

    #[must_use]
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

    #[must_use]
    pub fn is_rejected(&self, path: &str) -> bool {
        let lower = path.to_ascii_lowercase();
        self.paths.reject.iter().any(|m| m.matches(&lower))
    }

    #[must_use]
    pub fn is_noise(&self, path: &str) -> bool {
        let lower = path.to_ascii_lowercase();
        self.paths.reject.iter().any(|m| m.matches(&lower))
            || self.paths.noise.iter().any(|m| m.matches(&lower))
    }

    #[must_use]
    pub const fn min_fuzzy_score(&self) -> f32 {
        self.rerank.thresholds.min_fuzzy_score
    }

    #[must_use]
    pub const fn min_semantic_score(&self) -> f32 {
        self.rerank.thresholds.min_semantic_score
    }

    #[must_use]
    pub const fn rerank_config(&self) -> &RerankConfig {
        &self.rerank
    }

    #[must_use]
    pub const fn graph_nodes(&self) -> &GraphNodesConfig {
        &self.graph_nodes
    }

    #[must_use]
    pub const fn embedding(&self) -> &EmbeddingTemplates {
        &self.embedding
    }

    #[must_use]
    pub const fn experts(&self) -> &ExpertsConfig {
        &self.experts
    }

    #[must_use]
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
        if let Some(schema_version) = raw.schema_version {
            if schema_version != 1 {
                return Err(anyhow!(
                    "profile.schema_version {schema_version} is not supported (expected 1)"
                ));
            }
        }

        let name = raw
            .name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| fallback_name.to_string());
        let description = raw.description;
        let paths = PathRules::from_raw(raw.paths, raw.must_hit)?;
        let rerank = RerankConfig::from_raw(raw.rerank);
        let graph_nodes = GraphNodesConfig::from_raw(raw.graph_nodes)?;
        let embedding = build_embedding_templates(raw.embedding)
            .with_context(|| format!("Invalid embedding template config for profile '{name}'"))?;
        let experts = ExpertsConfig::from_raw(raw.experts)
            .with_context(|| format!("Invalid experts config for profile '{name}'"))?;

        Ok(Self {
            name,
            description,
            paths,
            rerank,
            graph_nodes,
            embedding,
            experts,
        })
    }
}

fn validate_model_list(path: &str, models: &[String]) -> Result<()> {
    if models.is_empty() {
        return Err(anyhow!("{path} must not be empty"));
    }

    for (idx, model_id) in models.iter().enumerate() {
        if model_id.trim().is_empty() {
            return Err(anyhow!("{path}[{idx}] must not be empty"));
        }
    }
    Ok(())
}

fn build_embedding_templates(raw: Option<RawEmbeddingConfig>) -> Result<EmbeddingTemplates> {
    let mut templates = EmbeddingTemplates::default();
    let raw = raw.unwrap_or_default();

    if let Some(schema_version) = raw.schema_version {
        templates.schema_version = schema_version;
    }
    if let Some(max_chars) = raw.max_chars {
        templates.max_chars = max_chars;
    }

    if let Some(query) = raw.query {
        if let Some(default) = query.default {
            templates.query.default = default;
        }
        templates.query.identifier = query.identifier;
        templates.query.path = query.path;
        templates.query.conceptual = query.conceptual;
    }

    if let Some(doc) = raw.document {
        if let Some(default) = doc.default {
            templates.document.default = default;
        }
        templates.document.code = doc.code;
        templates.document.docs = doc.docs;
        templates.document.config = doc.config;
        templates.document.test = doc.test;
    }

    if let Some(graph) = raw.graph_node {
        if let Some(default) = graph.default {
            templates.graph_node.default = default;
        }
    }

    templates
        .validate()
        .map_err(|e| anyhow!("Embedding templates validation failed: {e}"))?;
    Ok(templates)
}

impl GraphNodesConfig {
    fn from_raw(raw: Option<RawGraphNodesConfig>) -> Result<Self> {
        let defaults = Self::default();
        let raw = raw.unwrap_or_default();
        let enabled = raw.enabled.unwrap_or(defaults.enabled);

        let weight = raw.weight.unwrap_or(defaults.weight);
        if !(0.0..=1.0).contains(&weight) {
            return Err(anyhow!(
                "graph_nodes.weight must be in [0.0, 1.0] (got {weight})"
            ));
        }

        let top_k = raw.top_k.unwrap_or(defaults.top_k).clamp(1, 500);
        let max_neighbors_per_relation = raw
            .max_neighbors_per_relation
            .unwrap_or(defaults.max_neighbors_per_relation)
            .clamp(1, 200);

        Ok(Self {
            enabled,
            weight,
            top_k,
            max_neighbors_per_relation,
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
    fn from_raw(raw: Option<RawRerankConfig>) -> Self {
        let raw = raw.unwrap_or_default();
        Self {
            thresholds: merge_thresholds(raw.thresholds),
            bm25: merge_bm25(raw.bm25),
            boosts: merge_boosts(raw.boosts),
            must_hit: merge_rerank_must_hit(raw.must_hit),
        }
    }
}

impl Matcher {
    fn new(kind: MatchKind, needle: &str) -> Result<Self> {
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
            MatchKind::Glob => self.glob.as_ref().is_some_and(|g| g.is_match(haystack)),
        }
    }
}

fn build_weighted_matchers(raw: Vec<RawWeightedRule>) -> Result<Vec<WeightedMatcher>> {
    let mut matchers = Vec::with_capacity(raw.len());
    for rule in raw {
        if rule.pattern.trim().is_empty() {
            continue;
        }
        let matcher = Matcher::new(rule.kind, &rule.pattern)?;
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
        matchers.push(Matcher::new(rule.kind, &rule.pattern)?);
    }
    Ok(matchers)
}

fn build_must_hit(raw: Vec<RawMustHitRule>) -> Result<Vec<MustHitRule>> {
    let mut rules = Vec::with_capacity(raw.len());
    for rule in raw {
        if rule.pattern.trim().is_empty() {
            continue;
        }
        let matcher = Matcher::new(rule.kind, &rule.pattern)?;
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
        other => Err(anyhow!("Base profile '{other}' not bundled")),
    }
}

fn merge_raw_profiles(mut base: RawProfile, overlay: RawProfile) -> RawProfile {
    let mut paths = RawPathRules::default();
    paths.boost.extend(base.paths.boost);
    paths.boost.extend(overlay.paths.boost);
    paths.penalty.extend(base.paths.penalty);
    paths.penalty.extend(overlay.paths.penalty);
    paths.reject.extend(base.paths.reject);
    paths.reject.extend(overlay.paths.reject);
    paths.noise.extend(base.paths.noise);
    paths.noise.extend(overlay.paths.noise);

    let mut must_hit = Vec::new();
    must_hit.extend(base.must_hit);
    must_hit.extend(overlay.must_hit);

    let rerank = match (base.rerank.take(), overlay.rerank) {
        (Some(base_cfg), Some(overlay_cfg)) => Some(merge_rerank(Some(base_cfg), overlay_cfg)),
        (Some(base_cfg), None) => Some(base_cfg),
        (None, Some(overlay_cfg)) => Some(overlay_cfg),
        (None, None) => None,
    };

    let graph_nodes = match (base.graph_nodes.take(), overlay.graph_nodes) {
        (Some(base_cfg), Some(overlay_cfg)) => Some(merge_graph_nodes_raw(base_cfg, overlay_cfg)),
        (Some(base_cfg), None) => Some(base_cfg),
        (None, Some(overlay_cfg)) => Some(overlay_cfg),
        (None, None) => None,
    };

    let embedding = match (base.embedding.take(), overlay.embedding) {
        (Some(base_cfg), Some(overlay_cfg)) => Some(merge_embedding_raw(base_cfg, overlay_cfg)),
        (Some(base_cfg), None) => Some(base_cfg),
        (None, Some(overlay_cfg)) => Some(overlay_cfg),
        (None, None) => None,
    };

    let experts = match (base.experts.take(), overlay.experts) {
        (Some(base_cfg), Some(overlay_cfg)) => Some(merge_experts_raw(base_cfg, overlay_cfg)),
        (Some(base_cfg), None) => Some(base_cfg),
        (None, Some(overlay_cfg)) => Some(overlay_cfg),
        (None, None) => None,
    };

    RawProfile {
        schema_version: overlay.schema_version.or(base.schema_version),
        // Do not inherit the base profile name when applying an overlay; the selected profile key
        // (`fallback_name` in `from_raw`) should become the effective name unless the overlay
        // explicitly sets one.
        name: overlay.name,
        description: overlay.description.or_else(|| base.description.take()),
        paths,
        must_hit,
        rerank,
        graph_nodes,
        embedding,
        experts,
    }
}

fn merge_experts_raw(mut base: RawExpertsConfig, overlay: RawExpertsConfig) -> RawExpertsConfig {
    base.schema_version = overlay.schema_version.or(base.schema_version);

    base.semantic = match (base.semantic.take(), overlay.semantic) {
        (Some(base_s), Some(overlay_s)) => Some(merge_semantic_experts_raw(base_s, overlay_s)),
        (Some(base_s), None) => Some(base_s),
        (None, Some(overlay_s)) => Some(overlay_s),
        (None, None) => None,
    };

    base.graph_nodes = match (base.graph_nodes.take(), overlay.graph_nodes) {
        (Some(base_g), Some(overlay_g)) => Some(merge_graph_node_experts_raw(base_g, overlay_g)),
        (Some(base_g), None) => Some(base_g),
        (None, Some(overlay_g)) => Some(overlay_g),
        (None, None) => None,
    };

    base
}

fn merge_semantic_experts_raw(
    mut base: RawSemanticExpertsConfig,
    overlay: RawSemanticExpertsConfig,
) -> RawSemanticExpertsConfig {
    base.default = overlay.default.or(base.default);
    base.identifier = overlay.identifier.or(base.identifier);
    base.path = overlay.path.or(base.path);
    base.conceptual = overlay.conceptual.or(base.conceptual);
    base
}

fn merge_graph_node_experts_raw(
    mut base: RawGraphNodeExpertsConfig,
    overlay: RawGraphNodeExpertsConfig,
) -> RawGraphNodeExpertsConfig {
    base.default = overlay.default.or(base.default);
    base
}

fn merge_embedding_raw(
    mut base: RawEmbeddingConfig,
    overlay: RawEmbeddingConfig,
) -> RawEmbeddingConfig {
    base.schema_version = overlay.schema_version.or(base.schema_version);
    base.max_chars = overlay.max_chars.or(base.max_chars);

    base.query = match (base.query.take(), overlay.query) {
        (Some(base_q), Some(overlay_q)) => Some(merge_query_templates_raw(base_q, overlay_q)),
        (Some(base_q), None) => Some(base_q),
        (None, Some(overlay_q)) => Some(overlay_q),
        (None, None) => None,
    };

    base.document = match (base.document.take(), overlay.document) {
        (Some(base_d), Some(overlay_d)) => Some(merge_document_templates_raw(base_d, overlay_d)),
        (Some(base_d), None) => Some(base_d),
        (None, Some(overlay_d)) => Some(overlay_d),
        (None, None) => None,
    };

    base.graph_node = match (base.graph_node.take(), overlay.graph_node) {
        (Some(base_g), Some(overlay_g)) => Some(merge_graph_node_templates_raw(base_g, overlay_g)),
        (Some(base_g), None) => Some(base_g),
        (None, Some(overlay_g)) => Some(overlay_g),
        (None, None) => None,
    };

    base
}

fn merge_query_templates_raw(
    mut base: RawQueryTemplates,
    overlay: RawQueryTemplates,
) -> RawQueryTemplates {
    base.default = overlay.default.or(base.default);
    base.identifier = overlay.identifier.or(base.identifier);
    base.path = overlay.path.or(base.path);
    base.conceptual = overlay.conceptual.or(base.conceptual);
    base
}

fn merge_document_templates_raw(
    mut base: RawDocumentTemplates,
    overlay: RawDocumentTemplates,
) -> RawDocumentTemplates {
    base.default = overlay.default.or(base.default);
    base.code = overlay.code.or(base.code);
    base.docs = overlay.docs.or(base.docs);
    base.config = overlay.config.or(base.config);
    base.test = overlay.test.or(base.test);
    base
}

fn merge_graph_node_templates_raw(
    mut base: RawGraphNodeTemplates,
    overlay: RawGraphNodeTemplates,
) -> RawGraphNodeTemplates {
    base.default = overlay.default.or(base.default);
    base
}

fn merge_graph_nodes_raw(
    mut base: RawGraphNodesConfig,
    overlay: RawGraphNodesConfig,
) -> RawGraphNodesConfig {
    let RawGraphNodesConfig {
        enabled,
        weight,
        top_k,
        max_neighbors_per_relation,
    } = overlay;
    base.enabled = enabled.or(base.enabled);
    base.weight = weight.or(base.weight);
    base.top_k = top_k.or(base.top_k);
    base.max_neighbors_per_relation =
        max_neighbors_per_relation.or(base.max_neighbors_per_relation);
    base
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
    let value: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(json_err) => {
            let utf8 = std::str::from_utf8(bytes).map_err(|err| anyhow!("{json_err}; {err}"))?;
            let toml_value: toml::Value = toml::from_str(utf8).map_err(|toml_err| {
                anyhow!(
                    "Profile is not valid JSON or TOML ({json_err}); TOML parse error: {toml_err}"
                )
            })?;
            serde_json::to_value(toml_value)
                .map_err(|err| anyhow!("Failed to convert TOML profile to JSON: {err}"))?
        }
    };

    validate_profile_value(&value)?;
    serde_json::from_value(value).map_err(|err| anyhow!("Profile parse error: {err}"))
}

#[allow(clippy::too_many_lines)]
fn validate_profile_value(value: &serde_json::Value) -> Result<()> {
    fn push_unknown(unknown: &mut Vec<String>, base: &str, key: &str) {
        if base.is_empty() {
            unknown.push(key.to_string());
        } else {
            unknown.push(format!("{base}.{key}"));
        }
    }

    fn validate_object_keys(
        unknown: &mut Vec<String>,
        obj: &serde_json::Map<String, serde_json::Value>,
        base: &str,
        allowed: &[&str],
    ) {
        for key in obj.keys() {
            if !allowed.iter().any(|a| a == &key.as_str()) {
                push_unknown(unknown, base, key);
            }
        }
    }

    const fn object_at(
        value: &serde_json::Value,
    ) -> Option<&serde_json::Map<String, serde_json::Value>> {
        match value {
            serde_json::Value::Object(obj) => Some(obj),
            _ => None,
        }
    }

    const fn array_at(value: &serde_json::Value) -> Option<&[serde_json::Value]> {
        match value {
            serde_json::Value::Array(arr) => Some(arr.as_slice()),
            _ => None,
        }
    }

    let serde_json::Value::Object(root) = value else {
        return Err(anyhow!("Profile config must be a JSON object"));
    };

    let mut unknown = Vec::new();

    // Top-level keys.
    validate_object_keys(
        &mut unknown,
        root,
        "",
        &[
            "schema_version",
            "name",
            "description",
            "paths",
            "rerank",
            "must_hit",
            "graph_nodes",
            "embedding",
            "experts",
        ],
    );

    // paths.*
    if let Some(paths) = root.get("paths").and_then(object_at) {
        validate_object_keys(
            &mut unknown,
            paths,
            "paths",
            &["boost", "penalty", "reject", "noise"],
        );
        for key in ["boost", "penalty"] {
            if let Some(arr) = root
                .get("paths")
                .and_then(object_at)
                .and_then(|p| p.get(key))
                .and_then(array_at)
            {
                for (idx, item) in arr.iter().enumerate() {
                    if let Some(obj) = object_at(item) {
                        validate_object_keys(
                            &mut unknown,
                            obj,
                            &format!("paths.{key}[{idx}]"),
                            &["pattern", "kind", "weight"],
                        );
                    }
                }
            }
        }
        for key in ["reject", "noise"] {
            if let Some(arr) = root
                .get("paths")
                .and_then(object_at)
                .and_then(|p| p.get(key))
                .and_then(array_at)
            {
                for (idx, item) in arr.iter().enumerate() {
                    if let Some(obj) = object_at(item) {
                        validate_object_keys(
                            &mut unknown,
                            obj,
                            &format!("paths.{key}[{idx}]"),
                            &["pattern", "kind"],
                        );
                    }
                }
            }
        }
    }

    // must_hit[]
    if let Some(arr) = root.get("must_hit").and_then(array_at) {
        for (idx, item) in arr.iter().enumerate() {
            if let Some(obj) = object_at(item) {
                validate_object_keys(
                    &mut unknown,
                    obj,
                    &format!("must_hit[{idx}]"),
                    &["pattern", "kind", "tokens", "boost"],
                );
            }
        }
    }

    // graph_nodes.*
    if let Some(graph_nodes) = root.get("graph_nodes").and_then(object_at) {
        validate_object_keys(
            &mut unknown,
            graph_nodes,
            "graph_nodes",
            &["enabled", "weight", "top_k", "max_neighbors_per_relation"],
        );
    }

    // embedding.*
    if let Some(embedding) = root.get("embedding").and_then(object_at) {
        validate_object_keys(
            &mut unknown,
            embedding,
            "embedding",
            &[
                "schema_version",
                "max_chars",
                "query",
                "document",
                "graph_node",
            ],
        );
        if let Some(query) = embedding.get("query").and_then(object_at) {
            validate_object_keys(
                &mut unknown,
                query,
                "embedding.query",
                &["default", "identifier", "path", "conceptual"],
            );
        }
        if let Some(doc) = embedding.get("document").and_then(object_at) {
            validate_object_keys(
                &mut unknown,
                doc,
                "embedding.document",
                &["default", "code", "docs", "config", "test"],
            );
        }
        if let Some(doc) = embedding.get("graph_node").and_then(object_at) {
            validate_object_keys(&mut unknown, doc, "embedding.graph_node", &["default"]);
        }
    }

    // experts.*
    if let Some(experts) = root.get("experts").and_then(object_at) {
        validate_object_keys(
            &mut unknown,
            experts,
            "experts",
            &["schema_version", "semantic", "graph_nodes"],
        );
        if let Some(semantic) = experts.get("semantic").and_then(object_at) {
            validate_object_keys(
                &mut unknown,
                semantic,
                "experts.semantic",
                &["default", "identifier", "path", "conceptual"],
            );
        }
        if let Some(graph_nodes) = experts.get("graph_nodes").and_then(object_at) {
            validate_object_keys(
                &mut unknown,
                graph_nodes,
                "experts.graph_nodes",
                &["default"],
            );
        }
    }

    // rerank.*
    if let Some(rerank) = root.get("rerank").and_then(object_at) {
        validate_object_keys(
            &mut unknown,
            rerank,
            "rerank",
            &["thresholds", "bm25", "boosts", "must_hit"],
        );
        if let Some(thresholds) = rerank.get("thresholds").and_then(object_at) {
            validate_object_keys(
                &mut unknown,
                thresholds,
                "rerank.thresholds",
                &["min_fuzzy_score", "min_semantic_score"],
            );
        }
        if let Some(bm25) = rerank.get("bm25").and_then(object_at) {
            validate_object_keys(&mut unknown, bm25, "rerank.bm25", &["k1", "b", "window"]);
        }
        if let Some(boosts) = rerank.get("boosts").and_then(object_at) {
            validate_object_keys(
                &mut unknown,
                boosts,
                "rerank.boosts",
                &["path", "symbol", "yaml_path", "bm25"],
            );
        }
        if let Some(must_hit) = rerank.get("must_hit").and_then(object_at) {
            validate_object_keys(&mut unknown, must_hit, "rerank.must_hit", &["base_bonus"]);
        }
    }

    if unknown.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "Profile config has unknown fields: {}",
            unknown.join(", ")
        ))
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
    use context_vector_store::QueryKind;

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
    fn path_boost_weight_skips_penalties() {
        let profile = SearchProfile::builtin("general").unwrap();
        assert!(profile.path_weight("docs/README.md") < 1.0);
        assert!((profile.path_boost_weight("docs/README.md") - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn profile_rejects_unknown_fields_with_paths() {
        let bytes = br#"
        {
          "schema_version": 1,
          "paths": {
            "boost": [],
            "unknown_key": []
          },
          "embedding": {
            "query": {
              "default": "Query: {text}",
              "oops": "broken"
            }
          }
        }
        "#;

        let err = SearchProfile::from_bytes("custom", bytes, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("paths.unknown_key"), "{msg}");
        assert!(msg.contains("embedding.query.oops"), "{msg}");
    }

    #[test]
    fn profile_rejects_unsupported_schema_version() {
        let bytes = br#"{ "schema_version": 999, "name": "x" }"#;
        let err = SearchProfile::from_bytes("custom", bytes, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("profile.schema_version"), "{msg}");
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
        let matcher = Matcher::new(MatchKind::Glob, "**/*.rs").unwrap();
        assert!(matcher.matches("src/lib.rs"));
        assert!(!matcher.matches("docs/readme.md"));
    }

    #[test]
    fn embedding_templates_reject_unknown_placeholders() {
        let bytes = br#"
        {
          "name": "bad",
          "embedding": {
            "query": { "default": "query:{nope}" }
          }
        }
        "#;

        let err = SearchProfile::from_bytes("bad", bytes, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Unsupported template placeholder"));
    }

    #[test]
    fn embedding_templates_merge_overrides_only_specified_fields() {
        let base_bytes = br#"
        {
          "name": "base",
          "embedding": {
            "max_chars": 1000,
            "query": { "default": "Q:{text}" },
            "document": { "default": "D:{text}" }
          }
        }
        "#;
        let overlay_bytes = br#"
        {
          "name": "overlay",
          "embedding": {
            "query": { "identifier": "ID:{text}" },
            "document": { "code": "CODE:{path}\n{text}" }
          }
        }
        "#;

        let base_raw = parse_raw(base_bytes).unwrap();
        let overlay_raw = parse_raw(overlay_bytes).unwrap();
        let merged = merge_raw_profiles(base_raw, overlay_raw);
        let profile = SearchProfile::from_raw(merged, "merged").unwrap();

        assert_eq!(profile.embedding().max_chars, 1000);
        assert_eq!(profile.embedding().query.default, "Q:{text}");
        assert_eq!(
            profile.embedding().query.identifier.as_deref(),
            Some("ID:{text}")
        );
        assert_eq!(profile.embedding().document.default, "D:{text}");
        assert_eq!(
            profile.embedding().document.code.as_deref(),
            Some("CODE:{path}\n{text}")
        );
    }

    #[test]
    fn embedding_templates_render_is_bounded() {
        let mut templates = EmbeddingTemplates {
            max_chars: 256,
            ..EmbeddingTemplates::default()
        };
        templates.query.default = "query:{text}".to_string();
        templates.validate().unwrap();

        let long = "a".repeat(1000);
        let out = templates
            .render_query(QueryKind::Conceptual, &long)
            .unwrap();
        assert!(out.len() <= 256);
        assert_eq!(out, format!("query:{}", "a".repeat(250)));
    }
}
