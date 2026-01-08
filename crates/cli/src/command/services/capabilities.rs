use crate::command::context::CommandContext;
use crate::command::domain::CommandOutcome;
use anyhow::Result;
use context_indexer::INDEX_STATE_SCHEMA_VERSION;
use context_protocol::{
    Capabilities, CapabilitiesServer, CapabilitiesVersions, DefaultBudgets, ToolNextAction,
    CAPABILITIES_SCHEMA_VERSION,
};
use serde_json::Value;

pub(crate) struct CapabilitiesService;

impl CapabilitiesService {
    pub async fn run(&self, _payload: Value, ctx: &CommandContext) -> Result<CommandOutcome> {
        let root_display = ctx
            .resolve_project(None)
            .await
            .ok()
            .map(|project| project.root.display().to_string());
        let budgets = DefaultBudgets::default();
        let start_args = root_display
            .as_deref()
            .map(|root| serde_json::json!({ "project": root, "max_chars": budgets.repo_onboarding_pack_max_chars }))
            .unwrap_or_else(|| serde_json::json!({ "max_chars": budgets.repo_onboarding_pack_max_chars }));

        let output = Capabilities {
            schema_version: CAPABILITIES_SCHEMA_VERSION,
            server: CapabilitiesServer {
                name: "context-cli".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            versions: CapabilitiesVersions {
                command_api: "v1".to_string(),
                mcp: "v2".to_string(),
                index_state: INDEX_STATE_SCHEMA_VERSION,
            },
            default_budgets: budgets,
            start_route: ToolNextAction {
                tool: "repo_onboarding_pack".to_string(),
                args: start_args,
                reason: "Start with a compact repo map + key docs (onboarding pack).".to_string(),
            },
        };

        CommandOutcome::from_value(output)
    }
}
