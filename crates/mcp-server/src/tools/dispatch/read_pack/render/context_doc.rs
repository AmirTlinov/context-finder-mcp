use super::super::intent_resolve::read_pack_intent_label;
use super::super::{ReadPackIntent, ReadPackResult, ResponseMode};
use super::section_render::{render_project_facts_summary, render_section};
use crate::tools::context_doc::ContextDocBuilder;

pub(in crate::tools::dispatch::read_pack) fn render_read_pack_context_doc(
    result: &ReadPackResult,
    response_mode: ResponseMode,
) -> String {
    let mut doc = ContextDocBuilder::new();
    match result.intent {
        ReadPackIntent::Memory => doc.push_answer("Project memory: stable facts + key snippets."),
        ReadPackIntent::Recall => doc.push_answer("Recall: answers + supporting snippets."),
        ReadPackIntent::File => doc.push_answer("File slice."),
        ReadPackIntent::Grep => doc.push_answer("Grep matches with context."),
        ReadPackIntent::Query => doc.push_answer("Query context pack."),
        ReadPackIntent::Onboarding => doc.push_answer("Onboarding snapshot (see notes)."),
        ReadPackIntent::Auto => doc.push_answer(&format!(
            "read_pack: intent={}",
            read_pack_intent_label(result.intent)
        )),
    }
    if response_mode != ResponseMode::Minimal {
        doc.push_root_fingerprint(result.meta.as_ref().and_then(|meta| meta.root_fingerprint));
    }

    render_project_facts_summary(&mut doc, &result.sections);

    for section in &result.sections {
        render_section(&mut doc, section, response_mode);
    }

    if response_mode == ResponseMode::Full && !result.next_actions.is_empty() {
        doc.push_blank();
        doc.push_note("next_actions:");
        let mut shown = 0usize;
        for action in result.next_actions.iter().take(4) {
            shown += 1;
            let args = serde_json::to_string(&action.args).unwrap_or_else(|_| "{}".to_string());
            doc.push_line(&format!(" - {} {args}", action.tool));
        }
        if result.next_actions.len() > shown {
            doc.push_line(&format!(
                " - … (showing {shown} of {})",
                result.next_actions.len()
            ));
        }
    }

    if let Some(cursor) = result.next_cursor.as_deref() {
        doc.push_cursor(cursor);
    } else if result.budget.truncated {
        doc.push_note("truncated=true (increase max_chars)");
    }

    doc.finish()
}
