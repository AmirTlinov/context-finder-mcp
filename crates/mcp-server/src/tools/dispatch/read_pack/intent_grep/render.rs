use super::super::super::router::cursor_alias::compact_cursor_alias;
use super::super::cursors::snippet_kind_for_path;
use super::super::{
    ReadPackContext, ReadPackNextAction, ReadPackSection, ReadPackSnippet, ResponseMode, ToolResult,
};
use super::params::GrepIntentParams;
use crate::tools::schemas::grep_context::GrepContextResult;
use serde_json::json;

pub(super) struct GrepFinalizeOutput<'a> {
    pub(super) sections: &'a mut Vec<ReadPackSection>,
    pub(super) next_actions: &'a mut Vec<ReadPackNextAction>,
    pub(super) next_cursor_out: &'a mut Option<String>,
}

pub(super) async fn finalize_grep_result(
    service: &super::super::ContextFinderService,
    ctx: &ReadPackContext,
    response_mode: ResponseMode,
    params: GrepIntentParams,
    mut result: GrepContextResult,
    out: GrepFinalizeOutput<'_>,
) -> ToolResult<()> {
    if let Some(cursor) = result.next_cursor.take() {
        let compact = compact_cursor_alias(service, cursor).await;
        result.next_cursor = Some(compact.clone());
        *out.next_cursor_out = Some(compact);
    } else {
        *out.next_cursor_out = None;
    }

    if response_mode == ResponseMode::Full {
        if let Some(next_cursor) = result.next_cursor.as_deref() {
            let GrepIntentParams {
                pattern,
                before,
                after,
                case_sensitive,
                grep_request,
                ..
            } = params;
            let file = grep_request.file;
            let file_pattern = grep_request.file_pattern;
            out.next_actions.push(ReadPackNextAction {
                tool: "read_pack".to_string(),
                args: json!({
                    "path": ctx.root_display.clone(),
                    "intent": "grep",
                    "pattern": pattern,
                    "file": file,
                    "file_pattern": file_pattern,
                    "before": before,
                    "after": after,
                    "case_sensitive": case_sensitive,
                    "max_chars": ctx.max_chars,
                    "cursor": next_cursor,
                }),
                reason: "Continue rg pagination (next page of hunks).".to_string(),
            });
        }
    }

    if response_mode == ResponseMode::Full {
        out.sections.push(ReadPackSection::GrepContext { result });
    } else {
        for hunk in result.hunks.iter().take(3) {
            let kind = if response_mode == ResponseMode::Minimal {
                None
            } else {
                Some(snippet_kind_for_path(&hunk.file))
            };
            out.sections.push(ReadPackSection::Snippet {
                result: ReadPackSnippet {
                    file: hunk.file.clone(),
                    start_line: hunk.start_line,
                    end_line: hunk.end_line,
                    content: hunk.content.clone(),
                    kind,
                    reason: Some(super::super::REASON_NEEDLE_GREP_HUNK.to_string()),
                    next_cursor: None,
                },
            });
        }
    }
    Ok(())
}
