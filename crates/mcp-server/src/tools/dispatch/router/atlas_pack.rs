use super::super::{
    CallToolResult, Content, ContextFinderService, McpError, ResponseMode, ToolMeta,
};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::{
    atlas_pack::compute_atlas_pack_result,
    schemas::{atlas_pack::AtlasPackRequest, worktree_pack::WorktreePackResult},
    worktree_pack::render_worktree_pack_block,
};

use super::cursor_alias::compact_cursor_alias;
use super::error::{
    attach_structured_content, internal_error_with_meta, invalid_request_with_meta,
    meta_for_request,
};

/// One-call atlas: meaning-first CP + worktree overview, optimized for agent onboarding.
pub(in crate::tools::dispatch) async fn atlas_pack(
    service: &ContextFinderService,
    request: AtlasPackRequest,
) -> Result<CallToolResult, McpError> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);

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

    let meta_for_output = if response_mode == ResponseMode::Minimal {
        ToolMeta {
            root_fingerprint: Some(context_indexer::root_fingerprint(&root_display)),
            ..ToolMeta::default()
        }
    } else {
        meta_for_request(service, request.path.as_deref()).await
    };

    let mut result = match compute_atlas_pack_result(&root, &root_display, &request).await {
        Ok(result) => result,
        Err(err) => {
            return Ok(internal_error_with_meta(
                format!("Error: {err:#}"),
                meta_for_output.clone(),
            ));
        }
    };
    result.meta = Some(meta_for_output.clone());

    if let Some(cursor) = result.worktrees_next_cursor.take() {
        result.worktrees_next_cursor = Some(compact_cursor_alias(service, cursor).await);
    }
    if let Some(actions) = result.next_actions.as_mut() {
        for action in actions {
            if action.tool != "worktree_pack" {
                continue;
            }
            let Some(obj) = action.args.as_object_mut() else {
                continue;
            };
            let Some(cursor) = obj.get("cursor").and_then(|v| v.as_str()) else {
                continue;
            };
            let compact = compact_cursor_alias(service, cursor.to_string()).await;
            obj.insert("cursor".to_string(), serde_json::json!(compact));
        }
    }

    let mut doc = ContextDocBuilder::new();
    doc.push_answer("atlas_pack");
    doc.push_root_fingerprint(meta_for_output.root_fingerprint);

    doc.push_note("meaning_pack:");
    doc.push_block_smart(&result.meaning_pack);
    if result.meaning_truncated {
        if let Some(truncation) = result.meaning_truncation.as_ref() {
            doc.push_note(&format!("meaning_truncated=true ({truncation:?})"));
        } else {
            doc.push_note("meaning_truncated=true");
        }
    }

    doc.push_blank();
    doc.push_note("worktrees:");
    if result.worktrees.is_empty() {
        doc.push_note("worktrees=0");
    } else {
        let worktree_view = WorktreePackResult {
            total_worktrees: None,
            returned: None,
            used_chars: None,
            limit: None,
            max_chars: None,
            truncated: result.worktrees_truncated,
            next_cursor: None,
            next_actions: None,
            meta: None,
            worktrees: result.worktrees.clone(),
        };
        doc.push_block_smart(&render_worktree_pack_block(&worktree_view));
        if result.worktrees_truncated {
            doc.push_note("worktrees_truncated=true");
        }
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

    let (text, bounded_truncated) = doc.finish_bounded(result.budget.max_chars);
    result.budget.used_chars = text.chars().count();
    result.budget.truncated = result.budget.truncated || bounded_truncated;

    let call_result = CallToolResult::success(vec![Content::text(text)]);
    Ok(attach_structured_content(
        call_result,
        &result,
        meta_for_output,
        "atlas_pack",
    ))
}
