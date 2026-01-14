use super::super::{
    compute_worktree_pack_result, render_worktree_pack_block, CallToolResult, Content,
    ContextFinderService, McpError, ResponseMode, ToolMeta, WorktreePackRequest,
};
use crate::tools::context_doc::ContextDocBuilder;

use super::cursor_alias::{compact_cursor_alias, expand_cursor_alias};
use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_cursor_with_meta,
    invalid_request_with_meta, meta_for_request,
};

/// Worktree atlas: list git worktrees/branches and what is being worked on (bounded + deterministic).
pub(in crate::tools::dispatch) async fn worktree_pack(
    service: &ContextFinderService,
    mut request: WorktreePackRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Minimal);

    if let Some(cursor) = request.cursor.as_deref() {
        match expand_cursor_alias(service, cursor).await {
            Ok(expanded) => request.cursor = Some(expanded),
            Err(message) => {
                let meta = if response_mode == ResponseMode::Full {
                    meta_for_request(service, request.path.as_deref()).await
                } else {
                    ToolMeta::default()
                };
                return Ok(invalid_cursor_with_meta(message, meta));
            }
        }
    }

    let (root, root_display) = match service
        .resolve_root_no_daemon_touch(request.path.as_deref())
        .await
    {
        Ok(value) => value,
        Err(message) => {
            let meta = if response_mode == ResponseMode::Minimal {
                ToolMeta::default()
            } else {
                meta_for_request(service, request.path.as_deref()).await
            };
            return Ok(invalid_request_with_meta(message, meta, None, Vec::new()));
        }
    };

    let mut result = match compute_worktree_pack_result(
        &root,
        &root_display,
        &request,
        request.cursor.as_deref(),
    )
    .await
    {
        Ok(value) => value,
        Err(err) => {
            let meta = if response_mode == ResponseMode::Minimal {
                ToolMeta::default()
            } else {
                meta_for_request(service, request.path.as_deref()).await
            };
            let message = format!("{err:#}");
            if message.starts_with("Invalid cursor:") {
                return Ok(invalid_cursor_with_meta(message, meta));
            }
            return Ok(internal_error_with_meta(message, meta));
        }
    };

    let meta_for_output = if response_mode == ResponseMode::Minimal {
        ToolMeta::default()
    } else {
        meta_for_request(service, request.path.as_deref()).await
    };
    result.meta = Some(meta_for_output.clone());

    if let Some(cursor) = result.next_cursor.take() {
        result.next_cursor = Some(compact_cursor_alias(service, cursor).await);
    }

    let mut doc = ContextDocBuilder::new();
    doc.push_answer("worktree_pack");
    doc.push_root_fingerprint(meta_for_output.root_fingerprint);
    doc.push_note("worktrees:");
    doc.push_block_smart(&render_worktree_pack_block(&result));
    if let Some(cursor) = result.next_cursor.as_deref() {
        doc.push_cursor(cursor);
    }
    if response_mode == ResponseMode::Full {
        if let Some(actions) = result.next_actions.as_ref() {
            if !actions.is_empty() {
                doc.push_blank();
                doc.push_note("next_actions:");
                for action in actions {
                    let mut args =
                        serde_json::to_string(&action.args).unwrap_or_else(|_| "{}".to_string());
                    if args.len() > 400 {
                        args.truncate(400);
                        args.push('…');
                    }
                    doc.push_note(&format!(
                        "next_action tool={} args={} reason={}",
                        action.tool, args, action.reason
                    ));
                }
            }
        }
    }

    let text = doc.finish();
    let call_result = CallToolResult::success(vec![Content::text(text)]);
    Ok(attach_structured_content(
        call_result,
        &result,
        meta_for_output,
        "worktree_pack",
    ))
}
