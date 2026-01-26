use super::super::{ReadPackRequest, ReadPackSnippetKind};
use super::limits::{
    MAX_RECALL_FILTER_PATHS, MAX_RECALL_FILTER_PATH_BYTES, MAX_RECALL_QUESTIONS,
    MAX_RECALL_QUESTION_BYTES, MAX_RECALL_QUESTION_CHARS, MAX_RECALL_TOPICS,
    MAX_RECALL_TOPIC_BYTES, MAX_RECALL_TOPIC_CHARS,
};
use std::path::Path;

pub(crate) fn trimmed_non_empty_str(input: Option<&str>) -> Option<&str> {
    input.map(str::trim).filter(|value| !value.is_empty())
}

pub(in crate::tools::dispatch::read_pack) fn trim_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max_chars {
            break;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

pub(in crate::tools::dispatch::read_pack) fn trim_utf8_bytes(s: &str, max_bytes: usize) -> String {
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

pub(in crate::tools::dispatch::read_pack) fn normalize_questions(
    request: &ReadPackRequest,
) -> Vec<String> {
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

pub(in crate::tools::dispatch::read_pack) fn normalize_topics(
    request: &ReadPackRequest,
) -> Option<Vec<String>> {
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

pub(in crate::tools::dispatch::read_pack) fn normalize_path_prefix_list(
    raw: Option<&Vec<String>>,
) -> Vec<String> {
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

pub(in crate::tools::dispatch::read_pack) fn normalize_optional_pattern(
    raw: Option<&str>,
) -> Option<String> {
    trimmed_non_empty_str(raw).map(|value| trim_utf8_bytes(value, MAX_RECALL_FILTER_PATH_BYTES))
}

pub(in crate::tools::dispatch::read_pack) fn snippet_kind_for_path(
    path: &str,
) -> ReadPackSnippetKind {
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
