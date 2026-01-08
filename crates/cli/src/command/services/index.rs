use crate::command::context::CommandContext;
use crate::command::domain::{
    parse_payload, CommandOutcome, Hint, HintKind, IndexPayload, IndexResponse,
};
use crate::command::infra::HealthPort;
use crate::command::warm;
use anyhow::Result;
use context_indexer::{ModelIndexSpec, MultiModelProjectIndexer};
use context_protocol::{DefaultBudgets, ToolNextAction};
use context_vector_store::{
    context_dir_for_project_root, current_model_id, ModelRegistry, QueryKind,
};
use std::collections::HashSet;

pub struct IndexService {
    health: HealthPort,
}

impl IndexService {
    pub fn new(health: HealthPort) -> Self {
        Self { health }
    }

    pub async fn run(
        &self,
        payload: serde_json::Value,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome> {
        let payload: IndexPayload = parse_payload(payload)?;
        let project_ctx = ctx.resolve_project(payload.path).await?;
        let _ = crate::heartbeat::ping(&project_ctx.root).await;
        let warm = warm::global_warmer().prewarm(&project_ctx.root).await;
        let templates = project_ctx.profile.embedding().clone();

        let primary_model_id = current_model_id().unwrap_or_else(|_| "bge-small".to_string());
        let mut models = Vec::new();
        let mut seen = HashSet::new();
        seen.insert(primary_model_id.clone());
        models.push(primary_model_id.clone());

        if payload.experts {
            let experts = project_ctx.profile.experts();
            for kind in [
                QueryKind::Identifier,
                QueryKind::Path,
                QueryKind::Conceptual,
            ] {
                for model_id in experts.semantic_models(kind) {
                    if seen.insert(model_id.clone()) {
                        models.push(model_id.clone());
                    }
                }
            }
        }

        for model_id in payload.models {
            if seen.insert(model_id.clone()) {
                models.push(model_id);
            }
        }

        // Validate requested models early (clear errors before we start indexing).
        let registry = ModelRegistry::from_env()?;
        for model_id in &models {
            registry.dimension(model_id).map_err(|e| {
                anyhow::anyhow!("Unknown or unsupported model_id '{model_id}': {e}")
            })?;
        }

        let specs: Vec<ModelIndexSpec> = models
            .iter()
            .map(|model_id| ModelIndexSpec::new(model_id.clone(), templates.clone()))
            .collect();
        let indexer = MultiModelProjectIndexer::new(&project_ctx.root).await?;
        let stats = indexer.index_models(&specs, payload.full).await?;
        let primary_index_path =
            crate::command::context::index_path_for_model(&project_ctx.root, &primary_model_id);
        let reason = if payload.full {
            "full_index"
        } else {
            "manual_index"
        };
        let health_snapshot = self
            .health
            .record_index(&project_ctx.root, &stats, reason)
            .await;

        let mut outcome = CommandOutcome::from_value(IndexResponse { stats })?;
        outcome.meta.index_updated = Some(true);
        outcome.meta.config_path = project_ctx.config_path;
        outcome.meta.profile = Some(project_ctx.profile_name.clone());
        outcome.meta.profile_path = project_ctx.profile_path.clone();
        outcome.meta.index_files =
            Some(outcome.data["stats"]["files"].as_u64().unwrap_or(0) as usize);
        outcome.meta.index_chunks =
            Some(outcome.data["stats"]["chunks"].as_u64().unwrap_or(0) as usize);
        outcome.meta.index_size_bytes = tokio::fs::metadata(&primary_index_path)
            .await
            .ok()
            .map(|m| m.len());
        let graph_cache_path =
            context_dir_for_project_root(&project_ctx.root).join("graph_cache.json");
        outcome.meta.graph_cache_size_bytes = tokio::fs::metadata(graph_cache_path)
            .await
            .ok()
            .map(|m| m.len());
        outcome.meta.warm = Some(warm.warmed);
        outcome.meta.warm_cost_ms = Some(warm.warm_cost_ms);
        outcome.meta.warm_graph_cache_hit = Some(warm.graph_cache_hit);
        let budgets = DefaultBudgets::default();
        outcome.next_actions.push(ToolNextAction {
            tool: "repo_onboarding_pack".to_string(),
            args: serde_json::json!({
                "project": project_ctx.root.display().to_string(),
                "max_chars": budgets.repo_onboarding_pack_max_chars
            }),
            reason: "Start with a compact repo map + key docs after indexing.".to_string(),
        });
        outcome.next_actions.push(ToolNextAction {
            tool: "context_pack".to_string(),
            args: serde_json::json!({
                "project": project_ctx.root.display().to_string(),
                "query": "project overview",
                "max_chars": budgets.context_pack_max_chars
            }),
            reason: "Build a bounded semantic overview after indexing.".to_string(),
        });
        if models.len() > 1 {
            outcome.hints.push(Hint {
                kind: HintKind::Info,
                text: format!("Indexed {} models: {}", models.len(), models.join(", ")),
            });
        }
        outcome.hints.extend(project_ctx.hints);
        match health_snapshot {
            Ok(snapshot) => {
                outcome.meta.health_last_success_ms = Some(snapshot.last_success_unix_ms);
                outcome.meta.health_p95_ms = snapshot.p95_duration_ms;
                outcome.meta.health_files_per_sec = snapshot.files_per_sec;
                outcome.meta.health_failure_count = snapshot.failure_count;
                outcome.hints.push(Hint {
                    kind: HintKind::Info,
                    text: format!(
                        "Health updated at {} ms (reason: {})",
                        snapshot.last_success_unix_ms, snapshot.reason
                    ),
                });
            }
            Err(err) => log::warn!("Failed to persist health snapshot: {err:#}"),
        }
        Ok(outcome)
    }
}
