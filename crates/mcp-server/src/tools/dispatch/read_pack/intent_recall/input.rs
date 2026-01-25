use super::super::cursors::{
    normalize_optional_pattern, normalize_path_prefix_list, normalize_questions, normalize_topics,
    trimmed_non_empty_str, ReadPackRecallCursorV1,
};
use super::super::recall_cursor::decode_recall_cursor;
use super::super::{
    call_error, invalid_cursor_with_meta_details, ContextFinderService, ReadPackContext,
    ReadPackRequest, ToolResult, CURSOR_VERSION,
};
use crate::tools::cursor::cursor_fingerprint;
use context_indexer::{root_fingerprint, ToolMeta};
use serde_json::json;

pub(super) struct RecallInput {
    pub(super) questions: Vec<String>,
    pub(super) topics: Option<Vec<String>>,
    pub(super) start_index: usize,
    pub(super) include_paths: Vec<String>,
    pub(super) exclude_paths: Vec<String>,
    pub(super) file_pattern: Option<String>,
    pub(super) prefer_code: Option<bool>,
    pub(super) include_docs: Option<bool>,
    pub(super) allow_secrets: bool,
}

pub(super) async fn resolve_recall_input(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
) -> ToolResult<RecallInput> {
    if let Some(cursor) = trimmed_non_empty_str(request.cursor.as_deref()) {
        let overrides = request.ask.is_some()
            || request.questions.is_some()
            || request.topics.is_some()
            || request
                .include_paths
                .as_ref()
                .is_some_and(|p| p.iter().any(|p| !p.trim().is_empty()))
            || request
                .exclude_paths
                .as_ref()
                .is_some_and(|p| p.iter().any(|p| !p.trim().is_empty()))
            || trimmed_non_empty_str(request.file_pattern.as_deref()).is_some()
            || request.prefer_code.is_some()
            || request.include_docs.is_some()
            || request.allow_secrets.is_some();
        if overrides {
            return Err(call_error(
                "invalid_cursor",
                "Cursor continuation does not allow overriding recall parameters",
            ));
        }

        let decoded: ReadPackRecallCursorV1 = decode_recall_cursor(service, cursor).await?;
        if decoded.v != CURSOR_VERSION || decoded.tool != "read_pack" || decoded.mode != "recall" {
            return Err(call_error("invalid_cursor", "Invalid cursor: wrong tool"));
        }
        let expected_root_hash = cursor_fingerprint(&ctx.root_display);
        let expected_root_fingerprint = root_fingerprint(&ctx.root_display);
        if let Some(hash) = decoded.root_hash {
            if hash != expected_root_hash {
                return Err(invalid_cursor_with_meta_details(
                    "Invalid cursor: different root",
                    ToolMeta {
                        root_fingerprint: Some(expected_root_fingerprint),
                        ..ToolMeta::default()
                    },
                    json!({
                        "expected_root_fingerprint": expected_root_fingerprint,
                        "cursor_root_fingerprint": Some(hash),
                    }),
                ));
            }
        } else if decoded.root.as_deref() != Some(ctx.root_display.as_str()) {
            let cursor_root_fingerprint = decoded.root.as_deref().map(root_fingerprint);
            return Err(invalid_cursor_with_meta_details(
                "Invalid cursor: different root",
                ToolMeta {
                    root_fingerprint: Some(expected_root_fingerprint),
                    ..ToolMeta::default()
                },
                json!({
                    "expected_root_fingerprint": expected_root_fingerprint,
                    "cursor_root_fingerprint": cursor_root_fingerprint,
                }),
            ));
        }

        return Ok(RecallInput {
            questions: decoded.questions,
            topics: decoded.topics,
            start_index: decoded.next_question_index,
            include_paths: decoded.include_paths,
            exclude_paths: decoded.exclude_paths,
            file_pattern: decoded.file_pattern,
            prefer_code: decoded.prefer_code,
            include_docs: decoded.include_docs,
            allow_secrets: decoded.allow_secrets,
        });
    }

    Ok(RecallInput {
        questions: normalize_questions(request),
        topics: normalize_topics(request),
        start_index: 0,
        include_paths: normalize_path_prefix_list(request.include_paths.as_ref()),
        exclude_paths: normalize_path_prefix_list(request.exclude_paths.as_ref()),
        file_pattern: normalize_optional_pattern(request.file_pattern.as_deref()),
        prefer_code: request.prefer_code,
        include_docs: request.include_docs,
        allow_secrets: request.allow_secrets.unwrap_or(false),
    })
}
