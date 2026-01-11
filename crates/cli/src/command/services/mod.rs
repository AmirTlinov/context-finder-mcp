mod batch;
mod capabilities;
mod compare;
mod config;
mod context;
mod eval;
mod index;
mod meaning;
mod repo_onboarding_pack;
mod search;
mod text_search;

pub(crate) use search::collect_chunks;

use crate::cache::CacheConfig;
use crate::command::context::CommandContext;
use crate::command::domain::{CommandAction, CommandOutcome};
use crate::command::infra::{CompareCacheAdapter, GraphCacheFactory, HealthPort};
use anyhow::Result;
use serde_json::Value;

pub struct Services {
    capabilities: capabilities::CapabilitiesService,
    compare: compare::CompareService,
    config: config::ConfigService,
    context: context::ContextService,
    eval: eval::EvalService,
    index: index::IndexService,
    meaning: meaning::MeaningService,
    repo_onboarding_pack: repo_onboarding_pack::RepoOnboardingPackService,
    search: search::SearchService,
    text_search: text_search::TextSearchService,
}

impl Services {
    pub fn new(cache_cfg: CacheConfig) -> Self {
        let cache = CompareCacheAdapter::new(cache_cfg.clone());
        let graph = GraphCacheFactory;
        let health = HealthPort;

        Self {
            capabilities: capabilities::CapabilitiesService,
            compare: compare::CompareService::new(cache.clone(), graph.clone(), health.clone()),
            config: config::ConfigService,
            context: context::ContextService,
            eval: eval::EvalService,
            index: index::IndexService::new(health.clone()),
            meaning: meaning::MeaningService,
            repo_onboarding_pack: repo_onboarding_pack::RepoOnboardingPackService,
            search: search::SearchService::new(graph, health, cache),
            text_search: text_search::TextSearchService,
        }
    }

    pub async fn route(
        &self,
        action: CommandAction,
        payload: Value,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome> {
        match action {
            CommandAction::Batch => batch::run(self, payload, ctx).await,
            _ => self.route_item(action, payload, ctx).await,
        }
    }

    async fn route_item(
        &self,
        action: CommandAction,
        payload: Value,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome> {
        match action {
            CommandAction::Capabilities => self.capabilities.run(payload, ctx).await,
            CommandAction::Index => self.index.run(payload, ctx).await,
            CommandAction::Search => self.search.basic(payload, ctx).await,
            CommandAction::SearchWithContext => self.search.with_context(payload, ctx).await,
            CommandAction::ContextPack => self.search.context_pack(payload, ctx).await,
            CommandAction::MeaningPack => self.meaning.meaning_pack(payload, ctx).await,
            CommandAction::MeaningFocus => self.meaning.meaning_focus(payload, ctx).await,
            CommandAction::TaskPack => self.search.task_pack(payload, ctx).await,
            CommandAction::TextSearch => self.text_search.run(payload, ctx).await,
            CommandAction::EvidenceFetch => self.meaning.evidence_fetch(payload, ctx).await,
            CommandAction::Batch => unreachable!("batch action is handled by route()"),
            CommandAction::GetContext => self.context.get(payload, ctx).await,
            CommandAction::ListSymbols => self.context.list_symbols(payload, ctx).await,
            CommandAction::ConfigRead => self.config.read(payload, ctx).await,
            CommandAction::CompareSearch => self.compare.run(payload, ctx).await,
            CommandAction::Map => self.context.map(payload, ctx).await,
            CommandAction::RepoOnboardingPack => self.repo_onboarding_pack.run(payload, ctx).await,
            CommandAction::Eval => self.eval.run(payload, ctx).await,
            CommandAction::EvalCompare => self.eval.compare(payload, ctx).await,
        }
    }
}
