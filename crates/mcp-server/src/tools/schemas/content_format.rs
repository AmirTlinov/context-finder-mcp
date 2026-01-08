use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// How the tool should render text payloads.
///
/// - `plain`: raw lines (most compact).
/// - `numbered`: prefix each line with `<line>: ` (agent-friendly referencing).
#[derive(
    Debug, Deserialize, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum ContentFormat {
    #[default]
    Plain,
    Numbered,
}
