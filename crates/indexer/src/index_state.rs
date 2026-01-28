use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const INDEX_STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Watermark {
    Git {
        #[serde(skip_serializing_if = "Option::is_none")]
        computed_at_unix_ms: Option<u64>,
        git_head: String,
        git_dirty: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        dirty_hash: Option<u64>,
    },
    Filesystem {
        #[serde(skip_serializing_if = "Option::is_none")]
        computed_at_unix_ms: Option<u64>,
        file_count: u64,
        max_mtime_ms: u64,
        total_bytes: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StaleReason {
    IndexMissing,
    IndexCorrupt,
    WatermarkMissing,
    GitHeadMismatch,
    GitDirtyMismatch,
    FilesystemChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReindexResult {
    Ok,
    BudgetExceeded,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct ReindexAttempt {
    pub attempted: bool,
    pub performed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ReindexResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct IndexSnapshot {
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub built_at_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watermark: Option<Watermark>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct IndexState {
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,
    pub model_id: String,
    pub profile: String,
    pub project_watermark: Watermark,
    pub index: IndexSnapshot,
    pub stale: bool,
    #[serde(default)]
    pub stale_reasons: Vec<StaleReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reindex: Option<ReindexAttempt>,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema)]
pub struct StaleAssessment {
    pub stale: bool,
    pub reasons: Vec<StaleReason>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalMode {
    Semantic,
    Hybrid,
    Lexical,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnchorPolicy {
    #[default]
    Auto,
    Off,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnchorKind {
    Quoted,
    Path,
    Identifier,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema, Default)]
pub struct ToolTrustMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieval_mode: Option<RetrievalMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_used: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_policy: Option<AnchorPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_detected: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_kind: Option<AnchorKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_primary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_hits: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_not_found: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema, Default)]
pub struct ToolMeta {
    #[serde(default)]
    pub index_state: Option<IndexState>,
    /// Stable fingerprint for the resolved project root.
    ///
    /// This allows multi-session clients to detect accidental cross-project context mixups
    /// without exposing filesystem paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_fingerprint: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<ToolTrustMeta>,
}

/// Stable 64-bit fingerprint for root identification fields.
///
/// This is used in `ToolMeta` so clients can detect accidental cross-project context mixups
/// without embedding raw filesystem paths in responses.
#[must_use]
pub fn root_fingerprint(root_display: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(root_display.as_bytes());
    let digest = hasher.finalize();
    u64::from_be_bytes([
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
    ])
}

#[must_use]
pub fn assess_staleness(
    project_watermark: &Watermark,
    index_exists: bool,
    index_corrupt: bool,
    index_watermark: Option<&Watermark>,
) -> StaleAssessment {
    let mut reasons = Vec::new();

    if !index_exists {
        reasons.push(StaleReason::IndexMissing);
    }
    if index_corrupt {
        reasons.push(StaleReason::IndexCorrupt);
    }

    match index_watermark {
        None => {
            if index_exists {
                reasons.push(StaleReason::WatermarkMissing);
            }
        }
        Some(index_mark) => match (index_mark, project_watermark) {
            (
                Watermark::Git {
                    git_head: idx_head,
                    git_dirty: idx_dirty,
                    dirty_hash: idx_hash,
                    ..
                },
                Watermark::Git {
                    git_head: cur_head,
                    git_dirty: cur_dirty,
                    dirty_hash: cur_hash,
                    ..
                },
            ) => {
                if idx_head != cur_head {
                    reasons.push(StaleReason::GitHeadMismatch);
                }
                if idx_dirty != cur_dirty {
                    reasons.push(StaleReason::GitDirtyMismatch);
                }
                if idx_hash != cur_hash {
                    reasons.push(StaleReason::FilesystemChanged);
                }
            }
            (
                Watermark::Filesystem {
                    file_count: idx_files,
                    max_mtime_ms: idx_mtime,
                    total_bytes: idx_bytes,
                    ..
                },
                Watermark::Filesystem {
                    file_count: cur_files,
                    max_mtime_ms: cur_mtime,
                    total_bytes: cur_bytes,
                    ..
                },
            ) => {
                if idx_files != cur_files || idx_mtime != cur_mtime || idx_bytes != cur_bytes {
                    reasons.push(StaleReason::FilesystemChanged);
                }
            }
            _ => reasons.push(StaleReason::FilesystemChanged),
        },
    }

    let stale = !reasons.is_empty();
    StaleAssessment { stale, reasons }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn git(head: &str, dirty: bool) -> Watermark {
        Watermark::Git {
            computed_at_unix_ms: None,
            git_head: head.to_string(),
            git_dirty: dirty,
            dirty_hash: None,
        }
    }

    fn git_with_hash(head: &str, dirty: bool, dirty_hash: Option<u64>) -> Watermark {
        Watermark::Git {
            computed_at_unix_ms: None,
            git_head: head.to_string(),
            git_dirty: dirty,
            dirty_hash,
        }
    }

    fn fs(files: u64, max_mtime_ms: u64, bytes: u64) -> Watermark {
        Watermark::Filesystem {
            computed_at_unix_ms: None,
            file_count: files,
            max_mtime_ms,
            total_bytes: bytes,
        }
    }

    #[test]
    fn stale_when_index_missing() {
        let out = assess_staleness(&git("abc", false), false, false, None);
        assert_eq!(out.stale, true);
        assert_eq!(out.reasons, vec![StaleReason::IndexMissing]);
    }

    #[test]
    fn stale_when_index_corrupt() {
        let out = assess_staleness(&git("abc", false), true, true, Some(&git("abc", false)));
        assert_eq!(out.stale, true);
        assert_eq!(out.reasons, vec![StaleReason::IndexCorrupt]);
    }

    #[test]
    fn stale_when_watermark_missing() {
        let out = assess_staleness(&git("abc", false), true, false, None);
        assert_eq!(out.stale, true);
        assert_eq!(out.reasons, vec![StaleReason::WatermarkMissing]);
    }

    #[test]
    fn stale_when_git_head_mismatch() {
        let out = assess_staleness(&git("bbb", false), true, false, Some(&git("aaa", false)));
        assert_eq!(out.stale, true);
        assert_eq!(out.reasons, vec![StaleReason::GitHeadMismatch]);
    }

    #[test]
    fn stale_when_git_dirty_mismatch() {
        let out = assess_staleness(&git("aaa", true), true, false, Some(&git("aaa", false)));
        assert_eq!(out.stale, true);
        assert_eq!(out.reasons, vec![StaleReason::GitDirtyMismatch]);
    }

    #[test]
    fn stale_when_git_dirty_hash_mismatch() {
        let out = assess_staleness(
            &git_with_hash("aaa", true, Some(1)),
            true,
            false,
            Some(&git_with_hash("aaa", true, Some(2))),
        );
        assert_eq!(out.stale, true);
        assert_eq!(out.reasons, vec![StaleReason::FilesystemChanged]);
    }

    #[test]
    fn stale_when_filesystem_changed() {
        let out = assess_staleness(&fs(10, 123, 50), true, false, Some(&fs(10, 124, 50)));
        assert_eq!(out.stale, true);
        assert_eq!(out.reasons, vec![StaleReason::FilesystemChanged]);
    }

    #[test]
    fn fresh_when_git_equal() {
        let out = assess_staleness(&git("aaa", false), true, false, Some(&git("aaa", false)));
        assert_eq!(out.stale, false);
        assert_eq!(out.reasons, Vec::<StaleReason>::new());
    }

    #[test]
    fn fresh_when_filesystem_equal() {
        let mark = fs(10, 123, 50);
        let out = assess_staleness(&mark, true, false, Some(&mark));
        assert_eq!(out.stale, false);
        assert_eq!(out.reasons, Vec::<StaleReason>::new());
    }
}
