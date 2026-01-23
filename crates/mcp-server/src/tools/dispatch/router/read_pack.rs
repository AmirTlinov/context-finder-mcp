use super::super::{ContextFinderService, McpError, ReadPackRequest};
use rmcp::model::CallToolResult;

pub(in crate::tools::dispatch) async fn read_pack(
    service: &ContextFinderService,
    request: ReadPackRequest,
) -> Result<CallToolResult, McpError> {
    super::super::read_pack::read_pack(service, request).await
}
