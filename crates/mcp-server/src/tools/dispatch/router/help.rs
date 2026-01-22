use super::super::{CallToolResult, Content, ContextFinderService, McpError};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::context_legend::ContextLegend;
use crate::tools::schemas::help::HelpRequest;
use serde_json::json;

/// Explain the `.context` envelope conventions.
///
/// This tool intentionally returns the `[LEGEND]` block so other tools can stay low-noise even in
/// `response_mode=full`.
pub(in crate::tools::dispatch) async fn help(
    _service: &ContextFinderService,
    request: HelpRequest,
) -> Result<CallToolResult, McpError> {
    let topic = request
        .topic
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    let mut doc = ContextDocBuilder::new();
    doc.push_answer("help: `.context` legend and usage notes");

    if topic.is_empty() || topic == "legend" || topic == "format" {
        doc.push_note(
            "The `[LEGEND]` block appears only in this tool (to keep other tools low-noise).",
        );
        doc.push_note("A: short answer line (tool-level summary).");
        doc.push_note(
            "R: reference anchor (file:line [+ optional label]); snippet lines may follow.",
        );
        doc.push_note("M: continuation cursor (pass back as `cursor` to continue).");
        doc.push_note("N: hint/metadata (scores, relationships, next-step guidance).");
        doc.push_note(
            "Quoted lines start with a single leading space (avoids collisions with A:/R:/N:/M:).",
        );
    }

    if topic.is_empty() || topic == "golden_path" || topic == "flow" {
        doc.push_blank();
        doc.push_note("Recommended flow (dense + deterministic): rg → cat.");
        doc.push_note("Use read_pack when you want one-call onboarding/recall with cursors.");
        doc.push_note(
            "Use context_pack when you want semantic hits + related halo under a strict budget.",
        );
        doc.push_note("Use batch for multi-step workflows and `$ref` piping (JSON).");
    }

    if topic.is_empty() || topic == "cheat" || topic == "cheatsheet" || topic == "code" {
        doc.push_blank();
        doc.push_note("Cheat-sheet (when searching code):");
        doc.push_note("Exact string → text_search (fast, bounded, filesystem-first).");
        doc.push_note("Regex + surrounding context → rg (merged hunks).");
        doc.push_note("Open the exact place you found → cat (line-bounded).");
        doc.push_note(
            "Natural-language 'where is X done?' → search (quick) or context_pack (with related).",
        );
        doc.push_note(
            "What calls/uses X? → trace (path), impact (fanout), explain (deps+dependents).",
        );
        doc.push_note("Many steps in one call → batch (JSON, $ref piping).");
    }

    if !topic.is_empty()
        && topic != "legend"
        && topic != "format"
        && topic != "golden_path"
        && topic != "flow"
        && topic != "cheat"
        && topic != "cheatsheet"
        && topic != "code"
    {
        doc.push_blank();
        doc.push_note(&format!("Unknown topic={topic}; showing default help."));
    }

    let data = doc.finish();
    let mut text = String::new();
    text.push_str(ContextLegend::TEXT);
    text.push_str(&data);

    // Batch tool expects machine-readable payloads; keep a compact structured variant available.
    let mut structured = ContextLegend::structured();
    if let Some(obj) = structured.as_object_mut() {
        obj.insert(
            "topic".to_string(),
            if topic.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(topic.clone())
            },
        );
        obj.insert(
            "recommended_flow".to_string(),
            json!([
                "rg → cat (deterministic navigation)",
                "read_pack (one-call onboarding/recall)",
                "context_pack (semantic hits + related halo)",
                "batch (multi-step workflows, $ref piping)",
            ]),
        );
        obj.insert(
            "cheat_sheet".to_string(),
            json!({
                "exact_string": "text_search",
                "regex_context": "rg",
                "open_reference": "cat",
                "natural_language_find": ["search", "context_pack"],
                "call_path": "trace",
                "impact_fanout": "impact",
                "symbol_details": "explain",
                "multi_step": "batch",
            }),
        );
    }

    let mut result = CallToolResult::success(vec![Content::text(text)]);
    result.structured_content = Some(structured);
    Ok(result)
}
