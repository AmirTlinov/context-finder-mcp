use super::super::super::cursors::trim_chars;
use super::super::super::ResponseMode;
use crate::tools::context_doc::ContextDocBuilder;

pub(super) fn render_context_pack(
    doc: &mut ContextDocBuilder,
    pack_value: &serde_json::Value,
    response_mode: ResponseMode,
) {
    let parsed: Result<context_search::ContextPackOutput, _> =
        serde_json::from_value(pack_value.clone());
    match parsed {
        Ok(pack) => {
            let primary = pack.items.iter().filter(|i| i.role == "primary").count();
            let related = pack.items.iter().filter(|i| i.role == "related").count();
            doc.push_note(&format!(
                "context_pack: query={} items={} (primary={} related={}) truncated={} dropped_items={}",
                trim_chars(&pack.query, 80),
                pack.items.len(),
                primary,
                related,
                pack.budget.truncated,
                pack.budget.dropped_items
            ));

            if response_mode == ResponseMode::Full {
                let per_item_chars = 700usize;
                for item in pack.items.iter().take(4) {
                    doc.push_ref_header(&item.file, item.start_line, Some(item.role.as_str()));
                    if let Some(symbol) = item.symbol.as_deref() {
                        doc.push_note(&format!("symbol={} score={:.3}", symbol, item.score));
                    } else {
                        doc.push_note(&format!("score={:.3}", item.score));
                    }
                    doc.push_block_smart(&trim_chars(&item.content, per_item_chars));
                    doc.push_blank();
                }
                if pack.items.len() > 4 {
                    doc.push_note(&format!(
                        "context_pack: … (showing 4 of {} items)",
                        pack.items.len()
                    ));
                    doc.push_blank();
                }

                if !pack.next_actions.is_empty() {
                    doc.push_note("context_pack next_actions:");
                    let mut shown = 0usize;
                    for action in pack.next_actions.iter().take(3) {
                        shown += 1;
                        let args = serde_json::to_string(&action.args)
                            .unwrap_or_else(|_| "{}".to_string());
                        doc.push_line(&format!(" - {} {args}", action.tool));
                    }
                    if pack.next_actions.len() > shown {
                        doc.push_line(&format!(
                            " - … (showing {shown} of {})",
                            pack.next_actions.len()
                        ));
                    }
                    doc.push_blank();
                }
            } else {
                doc.push_blank();
            }
        }
        Err(_) => {
            doc.push_note("context_pack: (unrecognized result shape)");
            doc.push_blank();
        }
    }
}
