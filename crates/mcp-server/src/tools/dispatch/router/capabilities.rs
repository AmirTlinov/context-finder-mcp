use super::super::{CallToolResult, Content, ContextFinderService};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::schemas::capabilities::{CapabilitiesRequest, CapabilitiesResult};
use context_indexer::INDEX_STATE_SCHEMA_VERSION;
use context_protocol::{
    Capabilities, CapabilitiesServer, CapabilitiesVersions, ToolNextAction,
    CAPABILITIES_SCHEMA_VERSION,
};
use serde_json::json;

use super::error::{
    attach_structured_content, invalid_request_with_root_context, meta_for_request,
};

/// Return tool capabilities and default budgets for self-directed clients.
pub(in crate::tools::dispatch) async fn capabilities(
    service: &ContextFinderService,
    request: CapabilitiesRequest,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let (root, root_display) = match service
        .resolve_root_no_daemon_touch_for_tool(request.path.as_deref(), "capabilities")
        .await
    {
        Ok(value) => value,
        Err(message) => {
            let meta = meta_for_request(service, request.path.as_deref()).await;
            return Ok(
                invalid_request_with_root_context(service, message, meta, None, Vec::new()).await,
            );
        }
    };

    let budgets = super::super::mcp_default_budgets();
    let start_route = ToolNextAction {
        tool: "atlas_pack".to_string(),
        args: json!({
            "path": root_display,
            "max_chars": budgets.read_pack_max_chars,
            "response_mode": "facts"
        }),
        reason: "Start with a bounded onboarding atlas (meaning CP + worktrees) via atlas_pack."
            .to_string(),
    };

    let output = Capabilities {
        schema_version: CAPABILITIES_SCHEMA_VERSION,
        server: CapabilitiesServer {
            name: "context-mcp".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        versions: CapabilitiesVersions {
            command_api: "v1".to_string(),
            mcp: "v2".to_string(),
            index_state: INDEX_STATE_SCHEMA_VERSION,
        },
        default_budgets: budgets,
        start_route,
    };

    let result = CapabilitiesResult {
        capabilities: output,
        meta: service.tool_meta(&root).await,
    };

    let mut doc = ContextDocBuilder::new();
    doc.push_answer("capabilities: default budgets + start route");
    doc.push_root_fingerprint(result.meta.root_fingerprint);
    doc.push_note(&format!(
        "server: context-mcp {}",
        env!("CARGO_PKG_VERSION")
    ));
    doc.push_note(&format!(
        "versions: mcp=v2 command_api=v1 index_state=v{}",
        INDEX_STATE_SCHEMA_VERSION
    ));
    doc.push_note(&format!(
        "default_budgets.max_chars: {}",
        result.capabilities.default_budgets.max_chars
    ));
    doc.push_note(&format!(
        "default_budgets.read_pack_max_chars: {}",
        result.capabilities.default_budgets.read_pack_max_chars
    ));
    doc.push_note(&format!(
        "default_budgets.repo_onboarding_pack_max_chars: {}",
        result
            .capabilities
            .default_budgets
            .repo_onboarding_pack_max_chars
    ));
    doc.push_note(&format!(
        "default_budgets.context_pack_max_chars: {}",
        result.capabilities.default_budgets.context_pack_max_chars
    ));
    doc.push_note(&format!(
        "start: tool={}",
        result.capabilities.start_route.tool
    ));
    let output = CallToolResult::success(vec![Content::text(doc.finish())]);
    Ok(attach_structured_content(
        output,
        &result,
        result.meta.clone(),
        "capabilities",
    ))
}
