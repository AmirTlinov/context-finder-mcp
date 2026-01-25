use context_indexer::ToolMeta;
use context_protocol::{BudgetTruncation, ToolNextAction};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::response_mode::ResponseMode;

/// Control how `meaning_pack` returns results.
///
/// - `context`: default `.context` text output (CPV1).
/// - `markdown`: alias for `context` (common user expectation).
/// - `context_and_diagram`: `.context` text + an SVG diagram as an MCP `image` content block.
/// - `diagram`: SVG diagram only (lowest token usage, requires image-capable client/model).
#[derive(Debug, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MeaningPackOutputFormat {
    Context,
    ContextAndDiagram,
    Diagram,
}

impl<'de> Deserialize<'de> for MeaningPackOutputFormat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "context" | "markdown" => Ok(Self::Context),
            "context_and_diagram" => Ok(Self::ContextAndDiagram),
            "diagram" => Ok(Self::Diagram),
            other => Err(serde::de::Error::custom(format!(
                "Invalid output_format '{other}'. Allowed: context|markdown|context_and_diagram|diagram. Example: {{\"output_format\":\"context\"}}"
            ))),
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MeaningPackRequest {
    /// Project directory path.
    #[schemars(
        description = "Project directory path (defaults to session root; fallback: CONTEXT_ROOT/CONTEXT_PROJECT_ROOT; non-daemon fallback: cwd)."
    )]
    pub path: Option<String>,

    /// Natural-language query describing what to orient on.
    #[schemars(description = "Natural-language query describing what to orient on.")]
    pub query: String,

    /// Directory depth for grouping (default: 2).
    #[schemars(description = "Directory depth for grouping (1-4)")]
    pub map_depth: Option<usize>,

    /// Maximum number of directories to return (default: 12).
    #[schemars(description = "Maximum number of map entries returned")]
    pub map_limit: Option<usize>,

    /// Maximum UTF-8 characters for the entire meaning pack (default: 2000).
    #[schemars(description = "Maximum number of UTF-8 characters for the meaning pack")]
    pub max_chars: Option<usize>,

    /// Response mode:
    /// - "facts" (default): keeps meta/index_state for freshness, strips next_actions to reduce noise.
    /// - "full": includes meta/index_state and next_actions (when applicable).
    /// - "minimal": strips meta/index_state and next_actions to reduce noise.
    #[schemars(description = "Response mode: 'facts' (default), 'full', or 'minimal'")]
    pub response_mode: Option<ResponseMode>,

    /// Output format:
    /// - "context" (default): CPV1 `.context` output only.
    /// - "markdown": alias for "context".
    /// - "context_and_diagram": `.context` + `image/svg+xml` diagram.
    /// - "diagram": `image/svg+xml` diagram only.
    #[schemars(
        description = "Output format: 'context' (default), 'markdown' (alias), 'context_and_diagram', or 'diagram'"
    )]
    pub output_format: Option<MeaningPackOutputFormat>,

    /// Automatically build/refresh the semantic index when needed.
    #[schemars(
        description = "Automatically build or refresh the semantic index before executing (default: true)."
    )]
    pub auto_index: Option<bool>,

    /// Auto-index time budget in milliseconds when auto_index=true.
    #[schemars(description = "Auto-index time budget in milliseconds (default: 15000).")]
    pub auto_index_budget_ms: Option<u64>,
}

pub type MeaningPackTruncation = BudgetTruncation;
pub type MeaningPackNextAction = ToolNextAction;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MeaningPackBudget {
    pub max_chars: usize,
    pub used_chars: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<MeaningPackTruncation>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MeaningPackResult {
    pub version: u32,
    pub query: String,
    pub format: String,
    pub pack: String,
    pub budget: MeaningPackBudget,
    pub next_actions: Vec<MeaningPackNextAction>,
    #[serde(default)]
    pub meta: ToolMeta,
}
