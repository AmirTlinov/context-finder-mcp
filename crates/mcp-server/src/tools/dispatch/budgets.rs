use context_protocol::DefaultBudgets;

const DEFAULT_AUTO_INDEX_BUDGET_MS: u64 = 15_000;
const MIN_AUTO_INDEX_BUDGET_MS: u64 = 100;
const MAX_AUTO_INDEX_BUDGET_MS: u64 = 120_000;

const MCP_DEFAULT_MAX_CHARS: usize = 2_000;
// Onboarding tools are typically the first call in a fresh agent session; give them a bit more room
// by default so the first page contains both a map and at least one doc snippet.
const MCP_DEFAULT_ONBOARDING_MAX_CHARS: usize = 6_000;

pub(in crate::tools::dispatch) fn mcp_default_budgets() -> DefaultBudgets {
    DefaultBudgets {
        max_chars: MCP_DEFAULT_MAX_CHARS,
        read_pack_max_chars: MCP_DEFAULT_ONBOARDING_MAX_CHARS,
        repo_onboarding_pack_max_chars: MCP_DEFAULT_ONBOARDING_MAX_CHARS,
        context_pack_max_chars: MCP_DEFAULT_ONBOARDING_MAX_CHARS,
        batch_max_chars: MCP_DEFAULT_MAX_CHARS,
        cat_max_chars: MCP_DEFAULT_MAX_CHARS,
        rg_max_chars: MCP_DEFAULT_MAX_CHARS,
        ls_max_chars: MCP_DEFAULT_MAX_CHARS,
        tree_max_chars: MCP_DEFAULT_MAX_CHARS,
        file_slice_max_chars: MCP_DEFAULT_MAX_CHARS,
        grep_context_max_chars: MCP_DEFAULT_MAX_CHARS,
        list_files_max_chars: MCP_DEFAULT_MAX_CHARS,
        auto_index_budget_ms: DEFAULT_AUTO_INDEX_BUDGET_MS,
    }
}

pub(in crate::tools::dispatch) fn clamp_auto_index_budget_ms(budget_ms: u64) -> u64 {
    budget_ms.clamp(MIN_AUTO_INDEX_BUDGET_MS, MAX_AUTO_INDEX_BUDGET_MS)
}

#[derive(Clone, Copy, Debug)]
pub(in crate::tools) struct AutoIndexPolicy {
    pub(in crate::tools::dispatch) enabled: bool,
    pub(in crate::tools::dispatch) budget_ms: u64,
    pub(in crate::tools::dispatch) allow_missing_index_rebuild: bool,
    pub(in crate::tools::dispatch) budget_is_default: bool,
}

impl AutoIndexPolicy {
    fn with_budget_ms(
        budget_ms: u64,
        allow_missing_index_rebuild: bool,
        budget_is_default: bool,
    ) -> Self {
        let budget_ms = budget_ms.clamp(MIN_AUTO_INDEX_BUDGET_MS, MAX_AUTO_INDEX_BUDGET_MS);
        Self {
            enabled: true,
            budget_ms,
            allow_missing_index_rebuild,
            budget_is_default,
        }
    }

    pub(in crate::tools) fn from_request(
        auto_index: Option<bool>,
        auto_index_budget_ms: Option<u64>,
    ) -> Self {
        match auto_index {
            Some(false) => Self {
                enabled: false,
                budget_ms: DEFAULT_AUTO_INDEX_BUDGET_MS,
                allow_missing_index_rebuild: false,
                budget_is_default: true,
            },
            Some(true) => {
                let budget_is_default = auto_index_budget_ms.is_none();
                Self::with_budget_ms(
                    auto_index_budget_ms.unwrap_or(DEFAULT_AUTO_INDEX_BUDGET_MS),
                    true,
                    budget_is_default,
                )
            }
            None => {
                let mut policy = Self::semantic_default();
                if let Some(budget_ms) = auto_index_budget_ms {
                    policy =
                        Self::with_budget_ms(budget_ms, policy.allow_missing_index_rebuild, false);
                }
                policy
            }
        }
    }

    pub(in crate::tools) fn semantic_default() -> Self {
        // If we are running without a daemon (or in stub embedding mode), there is no background
        // warmup path. In that environment, allow a bounded inline build so semantic tools can
        // become usable without requiring an explicit `index` step.
        let daemon_disabled = std::env::var("CONTEXT_DISABLE_DAEMON")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let stub_embeddings = std::env::var("CONTEXT_EMBEDDING_MODE")
            .ok()
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("stub"));

        Self::with_budget_ms(
            DEFAULT_AUTO_INDEX_BUDGET_MS,
            daemon_disabled || stub_embeddings,
            true,
        )
    }
}
