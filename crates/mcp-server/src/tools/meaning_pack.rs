use anyhow::Result;
use context_indexer::ToolMeta;
use std::path::Path;

use context_meaning as meaning;

use super::schemas::meaning_pack::{MeaningPackBudget, MeaningPackRequest, MeaningPackResult};

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

    Ok(MeaningPackResult {
        version: engine.version,
        query: engine.query,
        format: engine.format,
        pack: engine.pack,
        budget: MeaningPackBudget {
            max_chars: engine.budget.max_chars,
            used_chars: engine.budget.used_chars,
            truncated: engine.budget.truncated,
            truncation: engine.budget.truncation,
        },
        next_actions: Vec::new(),
        meta: ToolMeta::default(),
    })
}
