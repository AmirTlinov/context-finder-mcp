use super::super::ResponseMode;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(in crate::tools::dispatch::read_pack) struct CursorHeader {
    pub(in crate::tools::dispatch::read_pack) v: u32,
    pub(in crate::tools::dispatch::read_pack) tool: String,
    #[serde(default)]
    pub(in crate::tools::dispatch::read_pack) mode: Option<String>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub(in crate::tools::dispatch::read_pack) struct ReadPackMemoryCursorV1 {
    pub(in crate::tools::dispatch::read_pack) v: u32,
    pub(in crate::tools::dispatch::read_pack) tool: String,
    pub(in crate::tools::dispatch::read_pack) mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) root_hash: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) max_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) response_mode: Option<ResponseMode>,
    pub(in crate::tools::dispatch::read_pack) next_candidate_index: usize,
    pub(in crate::tools::dispatch::read_pack) entrypoint_done: bool,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub(in crate::tools::dispatch::read_pack) struct ReadPackRecallCursorV1 {
    pub(in crate::tools::dispatch::read_pack) v: u32,
    pub(in crate::tools::dispatch::read_pack) tool: String,
    pub(in crate::tools::dispatch::read_pack) mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) root_hash: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) max_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) response_mode: Option<ResponseMode>,
    pub(in crate::tools::dispatch::read_pack) questions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) topics: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(in crate::tools::dispatch::read_pack) include_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(in crate::tools::dispatch::read_pack) exclude_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) file_pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) prefer_code: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) include_docs: Option<bool>,
    #[serde(default)]
    pub(in crate::tools::dispatch::read_pack) allow_secrets: bool,
    pub(in crate::tools::dispatch::read_pack) next_question_index: usize,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub(in crate::tools::dispatch::read_pack) struct ReadPackRecallCursorStoredV1 {
    pub(in crate::tools::dispatch::read_pack) v: u32,
    pub(in crate::tools::dispatch::read_pack) tool: String,
    pub(in crate::tools::dispatch::read_pack) mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) root_hash: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) max_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::tools::dispatch::read_pack) response_mode: Option<ResponseMode>,
    pub(in crate::tools::dispatch::read_pack) store_id: u64,
}
