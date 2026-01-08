use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Request for the `help` tool.
///
/// This tool is intentionally low-ceremony and stable: it exists so agents can discover the
/// `.context` envelope semantics (A:/N:/R:/M:) without paying that cost on every tool call.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Default)]
pub struct HelpRequest {
    /// Optional topic selector for future extensions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
}
