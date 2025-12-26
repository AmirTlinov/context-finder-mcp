use super::super::{
    compute_repo_onboarding_pack_result, CallToolResult, Content, ContextFinderService, McpError,
    RepoOnboardingPackRequest,
};

/// Repo onboarding pack (map + key docs slices + next actions).
pub(in crate::tools::dispatch) async fn repo_onboarding_pack(
    service: &ContextFinderService,
    request: RepoOnboardingPackRequest,
) -> Result<CallToolResult, McpError> {
    let (root, root_display) = match service.resolve_root(request.path.as_deref()).await {
        Ok(value) => value,
        Err(message) => return Ok(CallToolResult::error(vec![Content::text(message)])),
    };
    let mut result = match compute_repo_onboarding_pack_result(&root, &root_display, &request).await
    {
        Ok(result) => result,
        Err(err) => {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: {err:#}"
            ))]));
        }
    };
    result.meta = Some(service.tool_meta(&root).await);

    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&result).unwrap_or_default(),
    )]))
}
