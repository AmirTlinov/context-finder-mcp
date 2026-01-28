use clap::ValueEnum;

use crate::command::EvalCacheMode;

#[derive(Copy, Clone, ValueEnum)]
pub(crate) enum EmbedMode {
    Fast,
    Stub,
}

impl EmbedMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            EmbedMode::Fast => "fast",
            EmbedMode::Stub => "stub",
        }
    }
}

#[derive(Copy, Clone, ValueEnum)]
pub(crate) enum EvalCacheModeFlag {
    Warm,
    Cold,
}

impl EvalCacheModeFlag {
    pub(crate) const fn as_domain(self) -> EvalCacheMode {
        match self {
            EvalCacheModeFlag::Warm => EvalCacheMode::Warm,
            EvalCacheModeFlag::Cold => EvalCacheMode::Cold,
        }
    }
}

#[derive(Copy, Clone, ValueEnum)]
pub(crate) enum AnchorPolicyFlag {
    Auto,
    Off,
}

impl AnchorPolicyFlag {
    pub(crate) const fn as_domain(self) -> context_indexer::AnchorPolicy {
        match self {
            AnchorPolicyFlag::Auto => context_indexer::AnchorPolicy::Auto,
            AnchorPolicyFlag::Off => context_indexer::AnchorPolicy::Off,
        }
    }
}
