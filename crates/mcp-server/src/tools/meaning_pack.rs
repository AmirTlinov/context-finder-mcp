use anyhow::Result;
use context_indexer::ToolMeta;
use context_protocol::ToolNextAction;
use serde_json::json;
use std::path::Path;

use context_meaning as meaning;

use super::cpv1::{parse_cpv1_anchors, parse_cpv1_dict, parse_cpv1_evidence};
use super::schemas::meaning_pack::{MeaningPackBudget, MeaningPackRequest, MeaningPackResult};
use super::schemas::response_mode::ResponseMode;

pub(super) async fn compute_meaning_pack_result(
    root: &Path,
    root_display: &str,
    request: &MeaningPackRequest,
) -> Result<MeaningPackResult> {
    let engine_request = meaning::MeaningPackRequest {
        query: request.query.clone(),
        map_depth: request.map_depth,
        map_limit: request.map_limit,
        max_chars: request.max_chars,
    };
    let engine = meaning::meaning_pack(root, root_display, &engine_request).await?;

    let budget = MeaningPackBudget {
        max_chars: engine.budget.max_chars,
        used_chars: engine.budget.used_chars,
        truncated: engine.budget.truncated,
        truncation: engine.budget.truncation,
    };
    let next_actions = derive_meaning_pack_next_actions(root, &engine.pack, &budget, request);

    Ok(MeaningPackResult {
        version: engine.version,
        query: engine.query,
        format: engine.format,
        pack: engine.pack,
        budget,
        next_actions,
        meta: ToolMeta::default(),
    })
}

fn derive_meaning_pack_next_actions(
    root: &Path,
    pack: &str,
    budget: &MeaningPackBudget,
    request: &MeaningPackRequest,
) -> Vec<ToolNextAction> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    if response_mode != ResponseMode::Full {
        return Vec::new();
    }

    let dict = parse_cpv1_dict(pack);
    let ev_by_id = parse_cpv1_evidence(pack, &dict);
    let anchors = parse_cpv1_anchors(pack);

    let mut out: Vec<ToolNextAction> = Vec::new();

    // Best-effort: use the highest-priority anchor’s evidence as the primary next action.
    if let Some((_kind, ev_id)) = anchors.first() {
        if let Some(ptr) = ev_by_id.get(ev_id) {
            let mut evidence_fetch_args = json!({
                "items": [{
                    "file": ptr.file.clone(),
                    "start_line": ptr.start_line,
                    "end_line": ptr.end_line,
                    "source_hash": ptr.source_hash.clone(),
                }],
                "max_chars": 2000,
                "max_lines": 200,
                "strict_hash": false,
                "response_mode": "facts",
            });
            insert_optional_path(&mut evidence_fetch_args, &request.path);
            out.push(ToolNextAction {
                tool: "evidence_fetch".to_string(),
                args: evidence_fetch_args,
                reason: "Fetch exact source for the top anchor (evidence-backed).".to_string(),
            });

            let mut focus_args = json!({
                "focus": ptr.file.clone(),
                "query": "zoom into the top anchor and its nearby semantics",
                "max_chars": 2000,
                "response_mode": "facts",
            });
            insert_optional_path(&mut focus_args, &request.path);
            out.push(ToolNextAction {
                tool: "meaning_focus".to_string(),
                args: focus_args,
                reason: "Zoom into the anchor file/dir without leaving meaning mode.".to_string(),
            });
        }
    }

    if budget.truncated {
        // Guided retry: ask for a larger budget when the caller opted into `full` mode.
        let retry_max = budget.max_chars.saturating_mul(2).clamp(2_500, 20_000);
        let output_format = request
            .output_format
            .unwrap_or(super::schemas::meaning_pack::MeaningPackOutputFormat::Context);
        let mut retry_args = json!({
            "query": request.query.clone(),
            "max_chars": retry_max,
            "response_mode": "full",
            "output_format": output_format,
        });
        insert_optional_path(&mut retry_args, &request.path);
        out.push(ToolNextAction {
            tool: "meaning_pack".to_string(),
            args: retry_args,
            reason: "Retry with a larger max_chars because the pack was truncated.".to_string(),
        });
    }

    // Worktrees hint: if a repo keeps git worktrees under `.worktrees/`, surface a direct
    // drill-down action. This stays low-noise for most repos and reduces navigation friction.
    if root.join(".worktrees").is_dir() {
        let mut args = json!({
            "query": request.query.clone(),
            "max_chars": 2000,
            "response_mode": "full",
        });
        insert_optional_path(&mut args, &request.path);
        out.push(ToolNextAction {
            tool: "worktree_pack".to_string(),
            args,
            reason: "List worktrees/branches (repo uses .worktrees/)".to_string(),
        });
    }

    out
}

fn insert_optional_path(args: &mut serde_json::Value, path: &Option<String>) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    let Some(value) = path.as_ref() else {
        return;
    };
    obj.insert("path".to_string(), json!(value));
}
