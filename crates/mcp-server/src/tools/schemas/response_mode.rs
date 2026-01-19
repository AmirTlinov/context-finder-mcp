use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Control how much "helper" metadata the tool returns.
///
/// Defaults are tool-specific (tight-loop tools often default to `minimal`, while higher-level packs
/// like `read_pack` default to `facts`).
///
/// - `facts`: low-noise default (mostly payload, fewer helper fields).
/// - `full`: opt-in diagnostics (freshness meta/index_state, counters, next actions when applicable).
/// - `minimal`: smallest possible output (strips helper fields and diagnostics).
/// - `compact`: alias for `minimal` (common user expectation).
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseMode {
    Full,
    Facts,
    #[serde(alias = "compact")]
    Minimal,
}

#[cfg(test)]
mod tests {
    use super::ResponseMode;

    #[test]
    fn compact_alias_deserializes_as_minimal() {
        let parsed: ResponseMode = serde_json::from_str("\"compact\"").expect("deserialize");
        assert_eq!(parsed, ResponseMode::Minimal);
    }
}
