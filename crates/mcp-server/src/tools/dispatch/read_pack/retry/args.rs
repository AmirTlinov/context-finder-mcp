use super::super::intent_resolve::intent_label;
use super::super::{trimmed_non_empty_str, ReadPackContext, ReadPackIntent, ReadPackRequest};

pub(in crate::tools::dispatch::read_pack) fn build_retry_args(
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    intent: ReadPackIntent,
    max_chars: usize,
) -> serde_json::Value {
    let mut args = serde_json::Map::new();
    args.insert(
        "path".to_string(),
        serde_json::Value::String(ctx.root_display.clone()),
    );
    args.insert(
        "intent".to_string(),
        serde_json::Value::String(intent_label(intent).to_string()),
    );
    args.insert(
        "max_chars".to_string(),
        serde_json::Value::Number(max_chars.into()),
    );

    if let Some(mode) = request.response_mode {
        args.insert(
            "response_mode".to_string(),
            serde_json::to_value(mode).unwrap_or(serde_json::Value::Null),
        );
    }

    if let Some(cursor) = trimmed_non_empty_str(request.cursor.as_deref()) {
        args.insert(
            "cursor".to_string(),
            serde_json::Value::String(cursor.to_string()),
        );
    }
    if let Some(timeout_ms) = request.timeout_ms {
        args.insert(
            "timeout_ms".to_string(),
            serde_json::Value::Number(timeout_ms.into()),
        );
    }

    match intent {
        ReadPackIntent::File => {
            if let Some(file) = trimmed_non_empty_str(request.file.as_deref()) {
                args.insert(
                    "file".to_string(),
                    serde_json::Value::String(file.to_string()),
                );
            }
            if let Some(start_line) = request.start_line {
                args.insert(
                    "start_line".to_string(),
                    serde_json::Value::Number(start_line.into()),
                );
            }
            if let Some(max_lines) = request.max_lines {
                args.insert(
                    "max_lines".to_string(),
                    serde_json::Value::Number(max_lines.into()),
                );
            }
        }
        ReadPackIntent::Grep => {
            if let Some(pattern) = trimmed_non_empty_str(request.pattern.as_deref()) {
                args.insert(
                    "pattern".to_string(),
                    serde_json::Value::String(pattern.to_string()),
                );
            }
            if let Some(file_pattern) = trimmed_non_empty_str(request.file_pattern.as_deref()) {
                args.insert(
                    "file_pattern".to_string(),
                    serde_json::Value::String(file_pattern.to_string()),
                );
            }
            if let Some(before) = request.before {
                args.insert(
                    "before".to_string(),
                    serde_json::Value::Number(before.into()),
                );
            }
            if let Some(after) = request.after {
                args.insert("after".to_string(), serde_json::Value::Number(after.into()));
            }
            if let Some(case_sensitive) = request.case_sensitive {
                args.insert(
                    "case_sensitive".to_string(),
                    serde_json::Value::Bool(case_sensitive),
                );
            }
        }
        ReadPackIntent::Query => {
            if let Some(query) = trimmed_non_empty_str(request.query.as_deref()) {
                args.insert(
                    "query".to_string(),
                    serde_json::Value::String(query.to_string()),
                );
            }
            if let Some(file_pattern) = trimmed_non_empty_str(request.file_pattern.as_deref()) {
                args.insert(
                    "file_pattern".to_string(),
                    serde_json::Value::String(file_pattern.to_string()),
                );
            }
            if let Some(include_paths) = request.include_paths.as_ref() {
                let include_paths: Vec<serde_json::Value> = include_paths
                    .iter()
                    .map(|p| p.trim())
                    .filter(|p| !p.is_empty())
                    .map(|p| serde_json::Value::String(p.to_string()))
                    .collect();
                if !include_paths.is_empty() {
                    args.insert(
                        "include_paths".to_string(),
                        serde_json::Value::Array(include_paths),
                    );
                }
            }
            if let Some(exclude_paths) = request.exclude_paths.as_ref() {
                let exclude_paths: Vec<serde_json::Value> = exclude_paths
                    .iter()
                    .map(|p| p.trim())
                    .filter(|p| !p.is_empty())
                    .map(|p| serde_json::Value::String(p.to_string()))
                    .collect();
                if !exclude_paths.is_empty() {
                    args.insert(
                        "exclude_paths".to_string(),
                        serde_json::Value::Array(exclude_paths),
                    );
                }
            }
        }
        ReadPackIntent::Onboarding
        | ReadPackIntent::Memory
        | ReadPackIntent::Recall
        | ReadPackIntent::Auto => {}
    }

    serde_json::Value::Object(args)
}
