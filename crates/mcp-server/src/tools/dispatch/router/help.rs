use super::super::{CallToolResult, Content, ContextFinderService, McpError};
use crate::tools::catalog::TOOL_CATALOG;
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::context_legend::ContextLegend;
use crate::tools::schemas::help::HelpRequest;
use serde_json::json;

const HELP_TOPICS: &[&str] = &[
    "legend",
    "golden_path",
    "budgets",
    "cheat",
    "tools",
    "topics",
];

fn render_topics_list() -> String {
    HELP_TOPICS.join(", ")
}

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

    if topic.is_empty() || topic == "tools" || topic == "inventory" {
        doc.push_blank();
        doc.push_note("Tool inventory:");
        for tool in TOOL_CATALOG {
            doc.push_note(&format!("- {}: {}", tool.name, tool.summary));
        }
    }

    if topic.is_empty() || topic == "topics" {
        doc.push_blank();
        doc.push_note(&format!("Available topics: {}", render_topics_list()));
        doc.push_note("Example: help {\"topic\":\"tools\"}");
        doc.push_note("Example: help {\"topic\":\"budgets\"}");
    }

    if topic.is_empty() || topic == "golden_path" || topic == "flow" {
        doc.push_blank();
        doc.push_note("Day 0 onboarding: capabilities → atlas_pack → evidence_fetch.");
        doc.push_note("Daily memory: read_pack (defaults).");
        doc.push_note("Precise navigation: rg → cat (cursor-first).");
        doc.push_note("One-shot question pack: context_pack (semantic hits + related halo).");
        doc.push_note("Pipelines: batch v2 + $ref (multi-step in one call).");
        doc.push_note("See help {\"topic\":\"budgets\"} for max_chars presets.");
    }

    if topic.is_empty() || topic == "budgets" || topic == "budget" {
        doc.push_blank();
        doc.push_note("Recommended max_chars presets:");
        doc.push_note("~2000: tight-loop reads (cat/rg/ls/tree/text_search)");
        doc.push_note("~6000: packs (repo_onboarding_pack/read_pack/context_pack/atlas_pack)");
        doc.push_note("~20000: deep dives / big batches / CI troubleshooting");
        doc.push_note("Tip: prefer smaller budgets + cursor continuation (more deterministic).");
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
        && topic != "budgets"
        && topic != "budget"
        && topic != "tools"
        && topic != "inventory"
        && topic != "topics"
        && topic != "golden_path"
        && topic != "flow"
        && topic != "cheat"
        && topic != "cheatsheet"
        && topic != "code"
    {
        doc.push_blank();
        doc.push_note(&format!(
            "Unknown topic={topic}; available topics: {}",
            render_topics_list()
        ));
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
                "capabilities → atlas_pack (start route)",
                "rg → cat (deterministic navigation)",
                "read_pack (one-call onboarding/recall)",
                "context_pack (semantic hits + related halo)",
                "batch (multi-step workflows, $ref piping)",
            ]),
        );
        obj.insert("topics".to_string(), json!(HELP_TOPICS));
        obj.insert(
            "recommended_budgets".to_string(),
            json!({
                "tight_loop": 2000,
                "packs": 6000,
                "deep_dive": 20000
            }),
        );
        obj.insert(
            "tools".to_string(),
            json!(TOOL_CATALOG
                .iter()
                .map(|t| json!({ "name": t.name, "summary": t.summary }))
                .collect::<Vec<_>>()),
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
