use anyhow::Result;
use context_indexer::ToolMeta;
use context_protocol::ToolNextAction;
use serde_json::json;
use std::path::Path;

use context_meaning as meaning;

use super::cpv1::{parse_cpv1_anchors, parse_cpv1_dict, parse_cpv1_evidence};
use super::schemas::meaning_focus::{MeaningFocusBudget, MeaningFocusRequest, MeaningFocusResult};
use super::schemas::response_mode::ResponseMode;

pub(super) async fn compute_meaning_focus_result(
    root: &Path,
    root_display: &str,
    request: &MeaningFocusRequest,
) -> Result<MeaningFocusResult> {
    let engine_request = meaning::MeaningFocusRequest {
        focus: request.focus.clone(),
        query: request.query.clone(),
        map_depth: request.map_depth,
        map_limit: request.map_limit,
        max_chars: request.max_chars,
    };
    let engine = meaning::meaning_focus(root, root_display, &engine_request).await?;

    let budget = MeaningFocusBudget {
        max_chars: engine.budget.max_chars,
        used_chars: engine.budget.used_chars,
        truncated: engine.budget.truncated,
        truncation: engine.budget.truncation,
    };
    let next_actions = derive_meaning_focus_next_actions(&engine.pack, &budget, request);

    Ok(MeaningFocusResult {
        version: engine.version,
        query: engine.query,
        format: engine.format,
        pack: engine.pack,
        budget,
        next_actions,
        meta: ToolMeta::default(),
    })
}

fn derive_meaning_focus_next_actions(
    pack: &str,
    budget: &MeaningFocusBudget,
    request: &MeaningFocusRequest,
) -> Vec<ToolNextAction> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    if response_mode != ResponseMode::Full {
        return Vec::new();
    }

    let dict = parse_cpv1_dict(pack);
    let ev_by_id = parse_cpv1_evidence(pack, &dict);
    let anchors = parse_cpv1_anchors(pack);

    let mut out: Vec<ToolNextAction> = Vec::new();

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
                reason: "Fetch exact source for the focused meaning evidence.".to_string(),
            });
        }
    }

    if budget.truncated {
        let retry_max = budget.max_chars.saturating_mul(2).clamp(2_500, 20_000);
        let mut retry_args = json!({
            "focus": request.focus.clone(),
            "query": request.query.clone(),
            "max_chars": retry_max,
            "response_mode": "full",
        });
        insert_optional_path(&mut retry_args, &request.path);
        out.push(ToolNextAction {
            tool: "meaning_focus".to_string(),
            args: retry_args,
            reason: "Retry meaning_focus with a larger max_chars because the pack was truncated."
                .to_string(),
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
