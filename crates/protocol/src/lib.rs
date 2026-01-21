use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub mod path_filters;

pub const CAPABILITIES_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetTruncation {
    MaxChars,
    MaxLines,
    MaxMatches,
    MaxHunks,
    DocsLimit,
    Timeout,
    MaxItems,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct ToolNextAction {
    pub tool: String,
    pub args: serde_json::Value,
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct ErrorEnvelope {
    pub code: String,
    pub message: String,
    pub details: Option<serde_json::Value>,
    pub hint: Option<String>,
    #[serde(default)]
    pub next_actions: Vec<ToolNextAction>,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct DefaultBudgets {
    pub max_chars: usize,
    pub read_pack_max_chars: usize,
    pub repo_onboarding_pack_max_chars: usize,
    pub context_pack_max_chars: usize,
    pub batch_max_chars: usize,
    pub cat_max_chars: usize,
    pub rg_max_chars: usize,
    pub ls_max_chars: usize,
    pub tree_max_chars: usize,
    pub file_slice_max_chars: usize,
    pub grep_context_max_chars: usize,
    pub list_files_max_chars: usize,
    pub auto_index_budget_ms: u64,
}

impl Default for DefaultBudgets {
    fn default() -> Self {
        Self {
            max_chars: 20_000,
            read_pack_max_chars: 20_000,
            repo_onboarding_pack_max_chars: 20_000,
            context_pack_max_chars: 20_000,
            batch_max_chars: 20_000,
            cat_max_chars: 20_000,
            rg_max_chars: 20_000,
            ls_max_chars: 20_000,
            tree_max_chars: 20_000,
            file_slice_max_chars: 20_000,
            grep_context_max_chars: 20_000,
            list_files_max_chars: 20_000,
            auto_index_budget_ms: 15_000,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct CapabilitiesServer {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct CapabilitiesVersions {
    pub command_api: String,
    pub mcp: String,
    pub index_state: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct Capabilities {
    pub schema_version: u32,
    pub server: CapabilitiesServer,
    pub versions: CapabilitiesVersions,
    pub default_budgets: DefaultBudgets,
    pub start_route: ToolNextAction,
}

pub fn finalize_used_chars<T: Serialize>(
    value: &mut T,
    mut set_used: impl FnMut(&mut T, usize),
) -> Result<usize> {
    let mut used = 0usize;
    for _ in 0..8 {
        set_used(value, used);
        let raw = serde_json::to_string(value)?;
        let next = raw.chars().count();
        if next == used {
            set_used(value, next);
            return Ok(next);
        }
        used = next;
    }
    set_used(value, used);
    Ok(used)
}

pub fn enforce_max_chars<T: Serialize>(
    value: &mut T,
    max_chars: usize,
    mut set_used: impl FnMut(&mut T, usize),
    mut on_truncate: impl FnMut(&mut T),
    mut shrink: impl FnMut(&mut T) -> bool,
) -> Result<usize> {
    loop {
        let used = finalize_used_chars(value, |inner, used| set_used(inner, used))?;
        if used <= max_chars {
            return Ok(used);
        }
        on_truncate(value);
        if !shrink(value) {
            anyhow::bail!("budget exceeded (used_chars={used}, max_chars={max_chars})");
        }
    }
}

pub fn serialize_json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(Into::into)
}
