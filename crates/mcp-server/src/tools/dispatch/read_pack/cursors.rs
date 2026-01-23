use serde::Deserialize;
use std::path::Path;

use super::{ReadPackRequest, ReadPackSnippetKind, ResponseMode};

pub(super) fn trimmed_non_empty_str(input: Option<&str>) -> Option<&str> {
    input.map(str::trim).filter(|value| !value.is_empty())
}

#[derive(Debug, Deserialize)]
pub(super) struct CursorHeader {
    pub(super) v: u32,
    pub(super) tool: String,
    #[serde(default)]
    pub(super) mode: Option<String>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub(super) struct ReadPackMemoryCursorV1 {
    pub(super) v: u32,
    pub(super) tool: String,
    pub(super) mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) root_hash: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) max_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) response_mode: Option<ResponseMode>,
    pub(super) next_candidate_index: usize,
    pub(super) entrypoint_done: bool,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub(super) struct ReadPackRecallCursorV1 {
    pub(super) v: u32,
    pub(super) tool: String,
    pub(super) mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) root_hash: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) max_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) response_mode: Option<ResponseMode>,
    pub(super) questions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) topics: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) include_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) exclude_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) file_pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) prefer_code: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) include_docs: Option<bool>,
    #[serde(default)]
    pub(super) allow_secrets: bool,
    pub(super) next_question_index: usize,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub(super) struct ReadPackRecallCursorStoredV1 {
    pub(super) v: u32,
    pub(super) tool: String,
    pub(super) mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) root_hash: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) max_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) response_mode: Option<ResponseMode>,
    pub(super) store_id: u64,
}

pub(super) const MAX_RECALL_QUESTIONS: usize = 12;
pub(super) const MAX_RECALL_QUESTION_CHARS: usize = 220;
pub(super) const MAX_RECALL_QUESTION_BYTES: usize = 384;
pub(super) const MAX_RECALL_TOPICS: usize = 8;
pub(super) const MAX_RECALL_TOPIC_CHARS: usize = 80;
pub(super) const MAX_RECALL_TOPIC_BYTES: usize = 192;
pub(super) const DEFAULT_RECALL_SNIPPETS_PER_QUESTION: usize = 3;
pub(super) const MAX_RECALL_SNIPPETS_PER_QUESTION: usize = 5;

pub(super) fn trim_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max_chars {
            break;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

pub(super) fn trim_utf8_bytes(s: &str, max_bytes: usize) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= max_bytes {
        return trimmed.to_string();
    }

    let mut end = max_bytes.min(trimmed.len());
    while end > 0 && !trimmed.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    trimmed[..end].trim().to_string()
}

pub(super) fn normalize_questions(request: &ReadPackRequest) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(questions) = request.questions.as_ref() {
        for q in questions {
            let q = q.trim();
            if q.is_empty() {
                continue;
            }
            let q = trim_chars(q, MAX_RECALL_QUESTION_CHARS);
            out.push(trim_utf8_bytes(&q, MAX_RECALL_QUESTION_BYTES));
            if out.len() >= MAX_RECALL_QUESTIONS {
                break;
            }
        }
    }

    if out.is_empty() {
        if let Some(ask) = trimmed_non_empty_str(request.ask.as_deref()) {
            let lines: Vec<&str> = ask.lines().collect();
            if lines.len() > 1 {
                for line in lines {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let q = trim_chars(line, MAX_RECALL_QUESTION_CHARS);
                    out.push(trim_utf8_bytes(&q, MAX_RECALL_QUESTION_BYTES));
                    if out.len() >= MAX_RECALL_QUESTIONS {
                        break;
                    }
                }
            } else {
                let q = trim_chars(ask, MAX_RECALL_QUESTION_CHARS);
                out.push(trim_utf8_bytes(&q, MAX_RECALL_QUESTION_BYTES));
            }
        }
    }

    out
}

pub(super) fn normalize_topics(request: &ReadPackRequest) -> Option<Vec<String>> {
    let topics = request.topics.as_ref()?;

    let mut out = Vec::new();
    for topic in topics {
        let topic = topic.trim();
        if topic.is_empty() {
            continue;
        }
        let topic = trim_chars(topic, MAX_RECALL_TOPIC_CHARS);
        out.push(trim_utf8_bytes(&topic, MAX_RECALL_TOPIC_BYTES));
        if out.len() >= MAX_RECALL_TOPICS {
            break;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub(super) const MAX_RECALL_FILTER_PATHS: usize = 16;
pub(super) const MAX_RECALL_FILTER_PATH_BYTES: usize = 120;

pub(super) fn normalize_path_prefix_list(raw: Option<&Vec<String>>) -> Vec<String> {
    let Some(values) = raw else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        out.push(trim_utf8_bytes(value, MAX_RECALL_FILTER_PATH_BYTES));
        if out.len() >= MAX_RECALL_FILTER_PATHS {
            break;
        }
    }
    out
}

pub(super) fn normalize_optional_pattern(raw: Option<&str>) -> Option<String> {
    trimmed_non_empty_str(raw).map(|value| trim_utf8_bytes(value, MAX_RECALL_FILTER_PATH_BYTES))
}

pub(super) fn snippet_kind_for_path(path: &str) -> ReadPackSnippetKind {
    let normalized = path.replace('\\', "/");
    let file_name = Path::new(&normalized)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();

    if file_name.ends_with(".md")
        || file_name.ends_with(".mdx")
        || file_name.ends_with(".rst")
        || file_name.ends_with(".adoc")
        || file_name.ends_with(".txt")
        || file_name.ends_with(".context")
    {
        return ReadPackSnippetKind::Doc;
    }

    if file_name.starts_with('.') {
        return ReadPackSnippetKind::Config;
    }

    let ext = Path::new(&file_name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_lowercase();
    if matches!(
        ext.as_str(),
        "toml" | "json" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "properties" | "env"
    ) {
        return ReadPackSnippetKind::Config;
    }

    if file_name == "dockerfile"
        || file_name == "docker-compose.yml"
        || file_name == "docker-compose.yaml"
        || file_name == "makefile"
        || file_name == "justfile"
    {
        return ReadPackSnippetKind::Config;
    }

    ReadPackSnippetKind::Code
}
