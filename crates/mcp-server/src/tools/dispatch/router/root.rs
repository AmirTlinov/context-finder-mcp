use super::super::{
    CallToolResult, Content, ContextFinderService, McpError, RootGetRequest, RootGetResult,
    RootSetRequest, RootSetResult, ToolMeta,
};
use crate::tools::context_doc::ContextDocBuilder;
use context_indexer::root_fingerprint;

use super::error::invalid_request_with_meta;

pub(in crate::tools::dispatch) async fn root_get(
    service: &ContextFinderService,
    _request: RootGetRequest,
) -> Result<CallToolResult, McpError> {
    let (session_root, focus_file, workspace_roots, roots_pending, ambiguous, mismatch) = {
        let session = service.session.lock().await;
        let session_root = session.root_display();
        let focus_file = session.focus_file();
        let workspace_roots = session
            .mcp_workspace_roots()
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        (
            session_root,
            focus_file,
            workspace_roots,
            session.roots_pending(),
            session.mcp_roots_ambiguous(),
            session.root_mismatch_error().map(|s| s.to_string()),
        )
    };

    let mut meta = ToolMeta::default();
    if let Some(root_display) = session_root.as_deref() {
        meta.root_fingerprint = Some(root_fingerprint(root_display));
    }

    let mut doc = ContextDocBuilder::new();
    match session_root.as_deref() {
        Some(root) => doc.push_answer(&format!("root: {root}")),
        None => doc.push_answer("root: <unset>"),
    }
    if let Some(focus) = focus_file.as_deref() {
        doc.push_note(&format!("focus_file={focus}"));
    }
    if !workspace_roots.is_empty() {
        doc.push_note(&format!(
            "workspace_roots={}",
            workspace_roots
                .iter()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if roots_pending {
        doc.push_note("roots_pending=true");
    }
    if ambiguous {
        doc.push_note("workspace_roots_ambiguous=true");
    }
    if let Some(message) = mismatch.as_deref() {
        doc.push_note(&format!("root_mismatch_error={message}"));
    }
    doc.push_root_fingerprint(meta.root_fingerprint);

    let result = RootGetResult {
        session_root,
        focus_file,
        workspace_roots,
        roots_pending,
        workspace_roots_ambiguous: ambiguous,
        root_mismatch_error: mismatch,
        meta,
    };

    let mut out = CallToolResult::success(vec![Content::text(doc.finish())]);
    out.structured_content = Some(serde_json::json!(result));
    Ok(out)
}

pub(in crate::tools::dispatch) async fn root_set(
    service: &ContextFinderService,
    request: RootSetRequest,
) -> Result<CallToolResult, McpError> {
    let raw = request.path.trim();
    if raw.is_empty() {
        return Ok(invalid_request_with_meta(
            "Invalid path: empty",
            ToolMeta::default(),
            None,
            Vec::new(),
        ));
    }

    // Root switching is explicit user intent and must always be possible (even when the current
    // session root is pinned). Do not route through the generic `resolve_root` sticky-root logic.
    if !service.allow_cwd_root_fallback && !std::path::Path::new(raw).is_absolute() {
        return Ok(invalid_request_with_meta(
            "Invalid path: root_set requires an absolute path in shared-daemon mode",
            ToolMeta::default(),
            None,
            Vec::new(),
        ));
    }

    let root = match crate::tools::dispatch::root::canonicalize_root_path(std::path::Path::new(raw))
    {
        Ok(value) => value,
        Err(message) => {
            return Ok(invalid_request_with_meta(
                format!("Invalid path: {message}"),
                ToolMeta::default(),
                None,
                Vec::new(),
            ));
        }
    };
    let root_display = root.to_string_lossy().to_string();

    // Compute focus_file for absolute file hints (best-effort).
    let focus_file = if std::path::Path::new(raw).is_absolute() {
        let candidate = std::path::Path::new(raw);
        if let Ok(canonical) = candidate.canonicalize() {
            if let Ok(meta) = std::fs::metadata(&canonical) {
                if meta.is_file() {
                    canonical
                        .strip_prefix(&root)
                        .ok()
                        .and_then(crate::tools::dispatch::root::rel_path_string)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    {
        let mut session = service.session.lock().await;
        if !session.root_allowed_by_workspace(&root) {
            let roots_preview = crate::tools::dispatch::root::workspace_roots_preview(
                session.mcp_workspace_roots(),
            );
            return Ok(invalid_request_with_meta(
                format!(
                    "Invalid path: resolved root '{root_display}' is outside MCP workspace roots [{roots_preview}]."
                ),
                ToolMeta::default(),
                None,
                Vec::new(),
            ));
        }
        // Root switching is explicit user intent: persist even if the session root was previously
        // ambiguous (multi-root) or mismatched.
        session.set_root(root.clone(), root_display.clone(), focus_file.clone());
    }

    let (workspace_roots, roots_pending, ambiguous, mismatch) = {
        let session = service.session.lock().await;
        (
            session
                .mcp_workspace_roots()
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            session.roots_pending(),
            session.mcp_roots_ambiguous(),
            session.root_mismatch_error().map(|s| s.to_string()),
        )
    };

    let meta = ToolMeta {
        index_state: None,
        root_fingerprint: Some(root_fingerprint(&root_display)),
    };

    let mut doc = ContextDocBuilder::new();
    doc.push_answer(&format!("root: {root_display}"));
    if let Some(focus) = focus_file.as_deref() {
        doc.push_note(&format!("focus_file={focus}"));
    }
    if !workspace_roots.is_empty() {
        doc.push_note(&format!(
            "workspace_roots={}",
            workspace_roots
                .iter()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    doc.push_root_fingerprint(meta.root_fingerprint);

    let result = RootSetResult {
        session_root: root_display,
        focus_file,
        workspace_roots,
        roots_pending,
        workspace_roots_ambiguous: ambiguous,
        root_mismatch_error: mismatch,
        meta,
    };
    let mut out = CallToolResult::success(vec![Content::text(doc.finish())]);
    out.structured_content = Some(serde_json::json!(result));
    Ok(out)
}
