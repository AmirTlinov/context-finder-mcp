use super::super::router::error::{attach_meta, invalid_request_with_root_context};
use super::context::{build_context, ReadPackContext};
use super::intent_resolve::resolve_intent;
use super::{
    CallToolResult, ContextFinderService, ReadPackIntent, ReadPackRequest, ResponseMode,
    DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS,
};
use crate::tools::dispatch::AutoIndexPolicy;
use context_indexer::ToolMeta;

mod cursor;
mod path_hints;

use cursor::{apply_cursor_overrides, apply_cursor_root_fallback, expand_cursor_aliases};
use path_hints::apply_path_hints;

pub(super) struct PreparedReadPack {
    pub(super) request: ReadPackRequest,
    pub(super) ctx: ReadPackContext,
    pub(super) intent: ReadPackIntent,
    pub(super) response_mode: ResponseMode,
    pub(super) timeout_ms: u64,
    pub(super) meta: ToolMeta,
    pub(super) meta_for_output: Option<ToolMeta>,
    pub(super) semantic_index_fresh: bool,
    pub(super) allow_secrets: bool,
}

pub(super) async fn prepare_read_pack(
    service: &ContextFinderService,
    mut request: ReadPackRequest,
) -> Result<PreparedReadPack, CallToolResult> {
    // Expand compact cursor aliases early so routing and cursor-only continuation work.
    // Without this, `resolve_intent` would attempt to decode a non-base64 cursor alias directly.
    expand_cursor_aliases(service, &mut request).await?;

    // Cursor-only continuation: if the caller didn't pass `path`, we can fall back to the cursor's
    // embedded root *only when the current session has no established root*.
    // This is a safety boundary for multi-agent / multi-project usage.
    apply_cursor_root_fallback(service, &mut request).await?;

    // DX convenience: callers often pass `path` as a *subdirectory or file within the project*.
    // When the session already has a root, treat a relative `path` with no `file`/`file_pattern`
    // and no cursor as a file/file_pattern hint instead of switching the session root.
    apply_path_hints(service, &mut request).await;

    let mut hints: Vec<String> = Vec::new();
    if let Some(file) = request.file.as_deref() {
        hints.push(file.to_string());
    }
    if let Some(pattern) = request.file_pattern.as_deref() {
        if !pattern.trim().is_empty() {
            hints.push(pattern.to_string());
        }
    }
    let (root, root_display) = match service
        .resolve_root_with_hints_for_tool(request.path.as_deref(), &hints, "read_pack")
        .await
    {
        Ok(value) => value,
        Err(message) => {
            return Err(invalid_request_with_root_context(
                service,
                message,
                ToolMeta::default(),
                None,
                Vec::new(),
            )
            .await)
        }
    };
    let base_meta = service.tool_meta(&root).await;

    // Cursor-only continuation should preserve caller-selected budgets and response mode.
    apply_cursor_overrides(&mut request, &base_meta)?;

    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let ctx = match build_context(&request, root, root_display) {
        Ok(value) => value,
        Err(result) => return Err(attach_meta(result, base_meta.clone())),
    };
    let intent = match resolve_intent(&request) {
        Ok(value) => value,
        Err(result) => return Err(attach_meta(result, base_meta.clone())),
    };

    let timeout_ms = request
        .timeout_ms
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(1_000, MAX_TIMEOUT_MS);
    let meta = match intent {
        ReadPackIntent::Query => {
            service
                .tool_meta_with_auto_index(&ctx.root, AutoIndexPolicy::semantic_default())
                .await
        }
        _ => base_meta.clone(),
    };

    // Low-noise default: keep the response mostly project content.
    let provenance_meta = ToolMeta {
        root_fingerprint: meta.root_fingerprint,
        ..ToolMeta::default()
    };
    let meta_for_output = if response_mode == ResponseMode::Full {
        Some(meta.clone())
    } else {
        Some(provenance_meta)
    };

    let semantic_index_fresh = meta
        .index_state
        .as_ref()
        .is_some_and(|state| state.index.exists && !state.stale);
    let allow_secrets = request.allow_secrets.unwrap_or(false);

    Ok(PreparedReadPack {
        request,
        ctx,
        intent,
        response_mode,
        timeout_ms,
        meta,
        meta_for_output,
        semantic_index_fresh,
        allow_secrets,
    })
}
