use super::super::super::router::context_pack::context_pack;
use super::super::super::ContextPackRequest;
use super::super::candidates::is_disallowed_memory_file;
use super::super::cursors::{snippet_kind_for_path, trim_chars};
use super::super::recall_directives::{build_semantic_query, RecallQuestionMode};
use super::super::{
    ContextFinderService, ReadPackContext, ReadPackSnippet, ResponseMode,
    REASON_HALO_CONTEXT_PACK_PRIMARY,
};
use super::question::RecallQuestionContext;

pub(super) async fn semantic_snippets(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    response_mode: ResponseMode,
    question: &RecallQuestionContext,
    topics: Option<&Vec<String>>,
) -> Vec<ReadPackSnippet> {
    let avoid_semantic_for_structural =
        question.structural_intent.is_some() && question.question_mode != RecallQuestionMode::Deep;
    let is_ops = question.ops.is_some();
    if !question.allow_semantic
        || avoid_semantic_for_structural
        || (is_ops && question.question_mode != RecallQuestionMode::Deep)
    {
        return Vec::new();
    }

    let tool_result = context_pack(
        service,
        ContextPackRequest {
            path: Some(ctx.root_display.clone()),
            query: build_semantic_query(&question.clean_question, topics),
            format_version: None,
            anchor_policy: None,
            language: None,
            strategy: None,
            limit: Some(question.snippet_limit),
            max_chars: Some(
                question
                    .snippet_max_chars
                    .saturating_mul(question.snippet_limit)
                    .saturating_mul(2)
                    .clamp(1_000, 20_000),
            ),
            include_paths: if question.effective_include_paths.is_empty() {
                None
            } else {
                Some(question.effective_include_paths.clone())
            },
            exclude_paths: if question.effective_exclude_paths.is_empty() {
                None
            } else {
                Some(question.effective_exclude_paths.clone())
            },
            file_pattern: question.effective_file_pattern.clone(),
            max_related_per_primary: Some(1),
            include_docs: question.include_docs,
            prefer_code: question.prefer_code,
            related_mode: Some("focus".to_string()),
            response_mode: Some(ResponseMode::Minimal),
            trace: Some(false),
            auto_index: None,
            auto_index_budget_ms: None,
        },
    )
    .await;

    let Ok(tool_result) = tool_result else {
        return Vec::new();
    };
    if tool_result.is_error == Some(true) {
        return Vec::new();
    }

    let Some(value) = tool_result.structured_content.clone() else {
        return Vec::new();
    };
    let Some(items) = value.get("items").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    let mut snippets = Vec::new();
    for item in items.iter().take(question.snippet_limit) {
        let Some(file) = item.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(content) = item.get("content").and_then(|v| v.as_str()) else {
            continue;
        };
        let start_line = item.get("start_line").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        let start_line_u64 = start_line as u64;
        let end_line = item
            .get("end_line")
            .and_then(|v| v.as_u64())
            .unwrap_or(start_line_u64) as usize;
        if !question.allow_secrets && is_disallowed_memory_file(file) {
            continue;
        }
        snippets.push(ReadPackSnippet {
            file: file.to_string(),
            start_line,
            end_line,
            content: trim_chars(content, question.snippet_max_chars),
            kind: if response_mode == ResponseMode::Minimal {
                None
            } else {
                Some(snippet_kind_for_path(file))
            },
            reason: Some(REASON_HALO_CONTEXT_PACK_PRIMARY.to_string()),
            next_cursor: None,
        });
    }

    snippets
}
