use super::router::cursor_alias::{compact_cursor_alias, expand_cursor_alias};
use super::router::error::{
    attach_meta, attach_structured_content, invalid_cursor_with_meta_details, invalid_request_with,
    invalid_request_with_meta, meta_for_request, tool_error,
};
use super::{
    compute_file_slice_result, compute_grep_context_result, compute_repo_onboarding_pack_result,
    decode_cursor, encode_cursor, finalize_read_pack_budget, AutoIndexPolicy, CallToolResult,
    Content, ContextFinderService, ContextPackRequest, FileSliceCursorV1, FileSliceRequest,
    GrepContextComputeOptions, GrepContextCursorV1, GrepContextRequest, McpError, OverviewRequest,
    OverviewResult, ProjectFactsResult, ReadPackBudget, ReadPackIntent, ReadPackNextAction,
    ReadPackRecallResult, ReadPackRequest, ReadPackResult, ReadPackSection, ReadPackSnippet,
    ReadPackSnippetKind, ReadPackTruncation, RepoOnboardingPackRequest, ResponseMode,
    CURSOR_VERSION,
};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::cursor::cursor_fingerprint;
use crate::tools::file_slice::compute_onboarding_doc_slice;
use crate::tools::schemas::content_format::ContentFormat;
use context_indexer::{root_fingerprint, ToolMeta};
use context_protocol::ToolNextAction;
use context_search::QueryClassifier;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

mod context;
use context::{build_context, ReadPackContext};

mod anchor_scan;
mod candidates;
mod fs_scan;
mod project_facts;

use anchor_scan::{best_anchor_line_for_kind, memory_best_start_line};
use candidates::{
    collect_memory_file_candidates, collect_ops_file_candidates, config_candidate_score,
    is_disallowed_memory_file, ops_candidate_score, DEFAULT_MEMORY_FILE_CANDIDATES,
};
use project_facts::compute_project_facts;

const VERSION: u32 = 1;
const DEFAULT_MAX_CHARS: usize = 6_000;
const MIN_MAX_CHARS: usize = 400;
const MAX_MAX_CHARS: usize = 500_000;
const DEFAULT_GREP_CONTEXT: usize = 20;
const MAX_GREP_MATCHES: usize = 10_000;
const MAX_GREP_HUNKS: usize = 200;
// Agent-native default: keep tool calls snappy so the agent can stay in a tight loop.
// Callers can always opt in to longer work via `timeout_ms` (and/or `deep` for recall).
const DEFAULT_TIMEOUT_MS: u64 = 12_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const MAX_RECALL_INLINE_CURSOR_CHARS: usize = 1_200;

type ToolResult<T> = std::result::Result<T, CallToolResult>;

#[derive(Debug, Deserialize)]
struct CursorHeader {
    v: u32,
    tool: String,
    #[serde(default)]
    mode: Option<String>,
}

fn call_error(code: &'static str, message: impl Into<String>) -> CallToolResult {
    tool_error(code, message)
}

fn trimmed_non_empty_str(input: Option<&str>) -> Option<&str> {
    input.map(str::trim).filter(|value| !value.is_empty())
}

fn snippet_inner_max_chars(inner_max_chars: usize) -> usize {
    // Snippet-mode should stay small and leave room for envelope + cursor strings.
    (inner_max_chars / 2).clamp(200, 3_000).min(inner_max_chars)
}

fn truncate_vec<T>(values: &mut Vec<T>, max: usize) {
    if values.len() > max {
        values.truncate(max);
    }
}

fn trim_string_to_chars(input: &str, max_chars: usize) -> String {
    let max_chars = max_chars.max(1);
    let mut cut_byte = input.len();
    for (seen, (idx, _)) in input.char_indices().enumerate() {
        if seen == max_chars {
            cut_byte = idx;
            break;
        }
    }
    input[..cut_byte].to_string()
}

fn trim_project_facts_for_budget(
    mut facts: ProjectFactsResult,
    ctx: &ReadPackContext,
    response_mode: ResponseMode,
) -> ProjectFactsResult {
    // Under tight budgets, prefer a smaller but still useful facts section so we can always
    // include at least one payload snippet. Deterministic truncation only (no re-ordering).
    let budget = ctx.max_chars;
    let mut cap = if budget <= 1_200 {
        1usize
    } else if budget <= 3_000 {
        2usize
    } else if budget <= 6_000 {
        4usize
    } else {
        usize::MAX
    };
    if response_mode == ResponseMode::Minimal {
        cap = cap.min(2);
    }

    if cap == usize::MAX {
        return facts;
    }

    if budget <= 1_200 {
        // Ultra-tight mode: keep only the most stable, high-signal facts and leave room for at
        // least one snippet.
        truncate_vec(&mut facts.ecosystems, 1);
        truncate_vec(&mut facts.build_tools, 1);
        truncate_vec(&mut facts.ci, 1);
        truncate_vec(&mut facts.contracts, 1);
        // Entry points / config file paths can be long and are better shown as snippets in the
        // memory pack once the budget allows it.
        facts.entry_points.clear();
        facts.key_configs.clear();
        facts.key_dirs.clear();
        facts.modules.clear();
    } else {
        truncate_vec(&mut facts.ecosystems, cap.min(3));
        truncate_vec(&mut facts.build_tools, cap.min(4));
        truncate_vec(&mut facts.ci, cap.min(3));
        truncate_vec(&mut facts.contracts, cap.min(3));
        truncate_vec(&mut facts.key_dirs, cap.min(4));
        truncate_vec(&mut facts.modules, cap.min(6));
        truncate_vec(&mut facts.entry_points, cap.min(4));
        truncate_vec(&mut facts.key_configs, cap.min(6));
    }

    facts
}

fn trim_recall_sections_for_budget(
    result: &mut ReadPackResult,
    max_chars: usize,
) -> std::result::Result<(), String> {
    const MIN_SNIPPET_CHARS: usize = 80;
    const MAX_ITERS: usize = 64;

    // Best-effort fine trimming: prefer dropping extra snippets (or shrinking the last snippet)
    // over dropping entire questions/sections. This significantly improves "memory UX" under
    // tight budgets: agents get *some* answer for more questions per call.
    for _ in 0..MAX_ITERS {
        finalize_read_pack_budget(result).map_err(|err| format!("{err:#}"))?;
        if result.budget.used_chars <= max_chars {
            return Ok(());
        }

        // Find the last recall section (most likely to be the one we just appended).
        let mut found = false;
        for section in result.sections.iter_mut().rev() {
            let ReadPackSection::Recall { result: recall } = section else {
                continue;
            };
            found = true;

            if recall.snippets.len() > 1 {
                recall.snippets.pop();
                break;
            }

            if let Some(snippet) = recall.snippets.last_mut() {
                let cur_len = snippet.content.chars().count();
                if cur_len > MIN_SNIPPET_CHARS {
                    let next_len = (cur_len.saturating_mul(2) / 3).max(MIN_SNIPPET_CHARS);
                    snippet.content = trim_string_to_chars(&snippet.content, next_len);
                    break;
                }
            }
        }

        if !found {
            break;
        }
    }

    Ok(())
}

fn resolve_intent(request: &ReadPackRequest) -> ToolResult<ReadPackIntent> {
    let mut intent = request.intent.unwrap_or(ReadPackIntent::Auto);
    if !matches!(intent, ReadPackIntent::Auto) {
        return Ok(intent);
    }

    if let Some(cursor) = trimmed_non_empty_str(request.cursor.as_deref()) {
        let header: CursorHeader = decode_cursor(cursor)
            .map_err(|err| call_error("invalid_cursor", format!("Invalid cursor: {err}")))?;
        if header.v != CURSOR_VERSION {
            return Err(call_error(
                "invalid_cursor",
                "Invalid cursor: wrong version",
            ));
        }
        intent = match header.tool.as_str() {
            "cat" | "file_slice" => ReadPackIntent::File,
            "rg" | "grep" | "grep_context" => ReadPackIntent::Grep,
            "read_pack" => match header.mode.as_deref() {
                Some("recall") => ReadPackIntent::Recall,
                Some("memory") => ReadPackIntent::Memory,
                _ => {
                    return Err(call_error(
                        "invalid_cursor",
                        "Invalid cursor: unsupported read_pack cursor mode",
                    ))
                }
            },
            _ => {
                return Err(call_error(
                    "invalid_cursor",
                    "Invalid cursor: unsupported tool for read_pack",
                ))
            }
        };
        return Ok(intent);
    }

    fn looks_like_onboarding_prompt(text: &str) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        let lower = text.to_ascii_lowercase();

        // Prefer high-precision triggers. "How to" alone is too broad; require onboarding-ish
        // keywords that strongly correlate with repo orientation or setup/run instructions.
        let keywords = [
            "onboarding",
            "getting started",
            "quick start",
            "where to start",
            "repo structure",
            "project structure",
            "architecture",
            "entry point",
            "entrypoints",
            "how to run",
            "how do i run",
            "run tests",
            "how to test",
            "build and run",
            "setup",
            "install",
            "ci",
            "deploy",
            // Russian
            "онбординг",
            "с чего начать",
            "как запустить",
            "как собрать",
            "как установить",
            "как прогнать тест",
            "как запустить тест",
            "архитектура",
            "структура репозит",
            "точка входа",
        ];
        keywords.iter().any(|needle| lower.contains(needle))
    }

    let has_onboarding_signal = trimmed_non_empty_str(request.ask.as_deref())
        .is_some_and(looks_like_onboarding_prompt)
        || request.questions.as_ref().is_some_and(|qs| {
            qs.iter()
                .filter_map(|q| trimmed_non_empty_str(Some(q)))
                .any(looks_like_onboarding_prompt)
        })
        || trimmed_non_empty_str(request.query.as_deref())
            .is_some_and(looks_like_onboarding_prompt);
    if has_onboarding_signal {
        return Ok(ReadPackIntent::Onboarding);
    }

    if trimmed_non_empty_str(request.ask.as_deref()).is_some()
        || request
            .questions
            .as_ref()
            .is_some_and(|qs| qs.iter().any(|q| !q.trim().is_empty()))
    {
        return Ok(ReadPackIntent::Recall);
    }

    if trimmed_non_empty_str(request.query.as_deref()).is_some() {
        return Ok(ReadPackIntent::Query);
    }
    if trimmed_non_empty_str(request.pattern.as_deref()).is_some() {
        return Ok(ReadPackIntent::Grep);
    }
    if trimmed_non_empty_str(request.file.as_deref()).is_some() {
        return Ok(ReadPackIntent::File);
    }

    Ok(ReadPackIntent::Memory)
}

fn intent_label(intent: ReadPackIntent) -> &'static str {
    match intent {
        ReadPackIntent::Auto => "auto",
        ReadPackIntent::File => "file",
        ReadPackIntent::Grep => "grep",
        ReadPackIntent::Query => "query",
        ReadPackIntent::Onboarding => "onboarding",
        ReadPackIntent::Memory => "memory",
        ReadPackIntent::Recall => "recall",
    }
}

fn compute_min_envelope_chars(result: &ReadPackResult) -> ToolResult<usize> {
    let mut tmp = ReadPackResult {
        version: result.version,
        intent: result.intent,
        root: result.root.clone(),
        sections: Vec::new(),
        next_actions: Vec::new(),
        next_cursor: None,
        budget: ReadPackBudget {
            max_chars: result.budget.max_chars,
            used_chars: 0,
            truncated: true,
            truncation: Some(ReadPackTruncation::MaxChars),
        },
        meta: None,
    };
    finalize_read_pack_budget(&mut tmp)
        .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;
    Ok(tmp.budget.used_chars)
}

fn finalize_and_trim(
    mut result: ReadPackResult,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    intent: ReadPackIntent,
    response_mode: ResponseMode,
) -> ToolResult<ReadPackResult> {
    finalize_read_pack_budget(&mut result)
        .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;

    // Cursor-first UX: the presence of a continuation cursor means the response is incomplete
    // (paginated), even if we still fit under `max_chars`. Surface this deterministically via the
    // budget envelope so agents can rely on `truncated` as a single signal for "there is more".
    if result.next_cursor.is_some() && !result.budget.truncated {
        result.budget.truncated = true;
        if result.budget.truncation.is_none() {
            result.budget.truncation = Some(ReadPackTruncation::MaxItems);
        }
        // We mutated the envelope after computing `used_chars`; recompute so trimming decisions
        // stay correct under tight budgets.
        let _ = finalize_read_pack_budget(&mut result);
    }

    if result.budget.used_chars <= ctx.max_chars {
        return Ok(result);
    }

    result.budget.truncated = true;
    // If we exceeded max_chars, this is the dominant truncation reason even when we also have a
    // pagination cursor (max_items).
    result.budget.truncation = Some(ReadPackTruncation::MaxChars);

    // Recall pages should degrade by dropping snippets before dropping entire questions.
    if matches!(intent, ReadPackIntent::Recall) {
        let _ = trim_recall_sections_for_budget(&mut result, ctx.max_chars);
        let _ = finalize_read_pack_budget(&mut result);
        if result.budget.used_chars <= ctx.max_chars {
            return Ok(result);
        }
    }

    while result.budget.used_chars > ctx.max_chars && result.sections.len() > 1 {
        result.sections.pop();
        result.next_actions.clear();
        let _ = finalize_read_pack_budget(&mut result);
    }

    if result.budget.used_chars > ctx.max_chars {
        if !result.next_actions.is_empty() {
            result.next_actions.clear();
            let _ = finalize_read_pack_budget(&mut result);
        }
        if response_mode != ResponseMode::Full
            && result
                .meta
                .as_ref()
                .is_some_and(|meta| meta.index_state.is_some())
        {
            // Under very tight budgets, drop heavy diagnostics before sacrificing payload.
            result.meta = None;
            let _ = finalize_read_pack_budget(&mut result);
        }
        // Under extreme budgets we prefer to keep the continuation cursor (cheap) even if we must
        // drop all payload sections (expensive). This preserves an agent's tight-loop UX: the agent
        // can continue with a larger budget without losing pagination state.
        if result.budget.used_chars > ctx.max_chars {
            result.sections.clear();
            let _ = finalize_read_pack_budget(&mut result);
        }
        if result.budget.used_chars > ctx.max_chars && result.next_cursor.is_some() {
            result.next_cursor = None;
            let _ = finalize_read_pack_budget(&mut result);
        }
        if result.budget.used_chars > ctx.max_chars {
            let min_chars = compute_min_envelope_chars(&result)?;
            let suggested_max_chars = min_chars
                .max(ctx.max_chars.saturating_mul(2))
                .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);
            let retry_args = build_retry_args(ctx, request, intent, suggested_max_chars);
            return Err(invalid_request_with(
                format!("max_chars too small for read_pack response (min_chars={min_chars})"),
                Some(format!("Increase max_chars to at least {min_chars}.")),
                vec![ToolNextAction {
                    tool: "read_pack".to_string(),
                    args: retry_args,
                    reason: format!("Retry read_pack with max_chars >= {min_chars}."),
                }],
            ));
        }
    }

    if response_mode == ResponseMode::Full
        && result.budget.truncated
        && result.next_actions.is_empty()
        && result.next_cursor.is_none()
        && matches!(
            result.budget.truncation,
            Some(ReadPackTruncation::MaxChars | ReadPackTruncation::Timeout)
        )
    {
        ensure_retry_action(&mut result, ctx, request, intent);
        let _ = finalize_read_pack_budget(&mut result);
        if result.budget.used_chars > ctx.max_chars {
            result.next_actions.clear();
            let _ = finalize_read_pack_budget(&mut result);
        }
    }

    Ok(result)
}

const REASON_ANCHOR_FOCUS_FILE: &str = "anchor:focus_file";
const REASON_ANCHOR_DOC: &str = "anchor:doc";
const REASON_ANCHOR_ENTRYPOINT: &str = "anchor:entrypoint";
const REASON_NEEDLE_GREP_HUNK: &str = "needle:grep_hunk";
const REASON_NEEDLE_FILE_SLICE: &str = "needle:cat";
const REASON_HALO_CONTEXT_PACK_PRIMARY: &str = "halo:context_pack_primary";
const REASON_HALO_CONTEXT_PACK_RELATED: &str = "halo:context_pack_related";
const REASON_INTENT_FILE: &str = "intent:file";

async fn note_session_working_set_from_read_pack_result(
    service: &ContextFinderService,
    result: &ReadPackResult,
) {
    let mut files: Vec<&str> = Vec::new();
    for section in &result.sections {
        match section {
            ReadPackSection::Snippet { result } => files.push(&result.file),
            ReadPackSection::FileSlice { result } => files.push(&result.file),
            ReadPackSection::Recall { result } => {
                for snippet in &result.snippets {
                    files.push(&snippet.file);
                }
            }
            _ => {}
        }
    }

    if files.is_empty() {
        return;
    }

    let mut session = service.session.lock().await;
    for file in files {
        session.note_seen_snippet_file(file);
    }
}

fn snippet_reason_tier(reason: Option<&str>) -> u8 {
    let Some(reason) = reason else { return 0 };
    let lower = reason.trim().to_ascii_lowercase();
    if lower.starts_with("needle:") {
        return 3;
    }
    if lower.starts_with("halo:") {
        return 2;
    }
    if lower.starts_with("anchor:") {
        return 1;
    }
    0
}

fn snippet_kind_tier(kind: Option<ReadPackSnippetKind>) -> u8 {
    match kind {
        Some(ReadPackSnippetKind::Code) => 3,
        Some(ReadPackSnippetKind::Config) => 2,
        Some(ReadPackSnippetKind::Doc) => 1,
        None => 0,
    }
}

fn snippet_priority(snippet: &ReadPackSnippet) -> (u8, u8, usize) {
    let tier = snippet_reason_tier(snippet.reason.as_deref());
    let kind = snippet_kind_tier(snippet.kind);
    let span = snippet
        .end_line
        .saturating_sub(snippet.start_line)
        .saturating_add(1);
    (tier, kind, span)
}

fn snippet_overlap_len(a: &ReadPackSnippet, b: &ReadPackSnippet) -> Option<usize> {
    if a.file != b.file {
        return None;
    }
    let start = a.start_line.max(b.start_line);
    let end = a.end_line.min(b.end_line);
    if start > end {
        return None;
    }
    Some(end.saturating_sub(start).saturating_add(1))
}

fn snippet_is_focus_file(snippet: &ReadPackSnippet) -> bool {
    snippet.reason.as_deref() == Some(REASON_ANCHOR_FOCUS_FILE)
}

fn snippet_overlap_is_redundant(
    overlap_lines: usize,
    a: &ReadPackSnippet,
    b: &ReadPackSnippet,
) -> bool {
    if overlap_lines == 0 {
        return false;
    }
    let a_len = a.end_line.saturating_sub(a.start_line).saturating_add(1);
    let b_len = b.end_line.saturating_sub(b.start_line).saturating_add(1);
    let smaller = a_len.min(b_len).max(1);
    // Redundancy heuristic: if most of the smaller snippet is already covered, prefer a single
    // window (saves budget and reduces "needle spam" in facts mode).
    overlap_lines.saturating_mul(100) >= smaller.saturating_mul(70)
}

fn overlap_dedupe_snippet_sections(sections: &mut Vec<ReadPackSection>) {
    #[derive(Clone, Copy, Debug)]
    struct KeptSpan {
        idx: usize,
        start_line: usize,
        end_line: usize,
        priority: (u8, u8, usize),
    }

    let mut out: Vec<ReadPackSection> = Vec::with_capacity(sections.len());
    let mut kept_by_file: HashMap<String, Vec<KeptSpan>> = HashMap::new();

    for section in sections.drain(..) {
        let ReadPackSection::Snippet { result: snippet } = section else {
            out.push(section);
            continue;
        };

        // The memory "focus file" snippet is a UX anchor; never collapse it away.
        if snippet_is_focus_file(&snippet) {
            out.push(ReadPackSection::Snippet { result: snippet });
            continue;
        }

        let mut incoming = Some(snippet);
        let file = incoming.as_ref().expect("incoming set above").file.clone();
        let incoming_priority = snippet_priority(incoming.as_ref().expect("incoming set above"));
        let mut keep_incoming = true;

        let spans = kept_by_file.entry(file.clone()).or_default();
        for kept in spans.iter_mut() {
            let Some(existing_snippet) = (match out.get_mut(kept.idx) {
                Some(ReadPackSection::Snippet { result }) => Some(result),
                _ => None,
            }) else {
                continue;
            };

            if snippet_is_focus_file(existing_snippet) {
                continue;
            }

            let incoming_ref = incoming.as_ref().expect("incoming present");
            let Some(overlap) = snippet_overlap_len(existing_snippet, incoming_ref) else {
                continue;
            };

            // Exact duplicate span: keep the stronger one.
            if existing_snippet.start_line == incoming_ref.start_line
                && existing_snippet.end_line == incoming_ref.end_line
            {
                if incoming_priority > kept.priority {
                    if let Some(snippet) = incoming.take() {
                        *existing_snippet = snippet;
                    }
                    kept.start_line = existing_snippet.start_line;
                    kept.end_line = existing_snippet.end_line;
                    kept.priority = incoming_priority;
                }
                keep_incoming = false;
                break;
            }

            // Full containment: always drop the contained window (no information loss).
            let incoming_contains_existing = incoming_ref.start_line <= kept.start_line
                && incoming_ref.end_line >= kept.end_line;
            let existing_contains_incoming = kept.start_line <= incoming_ref.start_line
                && kept.end_line >= incoming_ref.end_line;

            if existing_contains_incoming {
                keep_incoming = false;
                break;
            }
            if incoming_contains_existing {
                if incoming_priority >= kept.priority {
                    if let Some(snippet) = incoming.take() {
                        *existing_snippet = snippet;
                    }
                    kept.start_line = existing_snippet.start_line;
                    kept.end_line = existing_snippet.end_line;
                    kept.priority = incoming_priority;
                }
                keep_incoming = false;
                break;
            }

            // Partial overlap: only collapse when it's mostly redundant; otherwise keep both so we
            // don't lose unique context (true merging is a future step).
            if !snippet_overlap_is_redundant(overlap, existing_snippet, incoming_ref) {
                continue;
            }

            if incoming_priority > kept.priority {
                if let Some(snippet) = incoming.take() {
                    *existing_snippet = snippet;
                }
                kept.start_line = existing_snippet.start_line;
                kept.end_line = existing_snippet.end_line;
                kept.priority = incoming_priority;
            }
            keep_incoming = false;
            break;
        }

        if keep_incoming {
            let Some(snippet) = incoming.take() else {
                continue;
            };
            let idx = out.len();
            spans.push(KeptSpan {
                idx,
                start_line: snippet.start_line,
                end_line: snippet.end_line,
                priority: incoming_priority,
            });
            out.push(ReadPackSection::Snippet { result: snippet });
        }
    }

    *sections = out;
}

fn strip_snippet_reasons_for_output(sections: &mut [ReadPackSection], keep_focus_file: bool) {
    for section in sections {
        match section {
            ReadPackSection::Snippet { result } => {
                if keep_focus_file && snippet_is_focus_file(result) {
                    continue;
                }
                result.reason = None;
            }
            ReadPackSection::Recall { result } => {
                for snippet in &mut result.snippets {
                    snippet.reason = None;
                }
            }
            _ => {}
        }
    }
}

fn read_pack_section_file(section: &ReadPackSection) -> Option<&str> {
    match section {
        ReadPackSection::Snippet { result } => {
            if result.reason.as_deref() == Some(REASON_ANCHOR_FOCUS_FILE) {
                None
            } else {
                Some(result.file.as_str())
            }
        }
        ReadPackSection::FileSlice { result } => Some(result.file.as_str()),
        ReadPackSection::ExternalMemory { .. } => None,
        ReadPackSection::Recall { .. } => None,
        ReadPackSection::ProjectFacts { .. } => None,
        ReadPackSection::Overview { .. } => None,
        ReadPackSection::GrepContext { .. } => None,
        ReadPackSection::ContextPack { .. } => None,
        ReadPackSection::RepoOnboardingPack { .. } => None,
    }
}

async fn repair_memory_cursor_after_trim(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    result: &mut ReadPackResult,
) {
    if result.next_cursor.is_some() {
        return;
    }

    let mut start_candidate_index = 0usize;
    let mut entrypoint_done_from_cursor = false;
    if let Some(cursor) = trimmed_non_empty_str(request.cursor.as_deref()) {
        if let Ok(decoded) = decode_cursor::<ReadPackMemoryCursorV1>(cursor) {
            let expected_root_hash = cursor_fingerprint(&ctx.root_display);
            let root_matches = if let Some(hash) = decoded.root_hash {
                hash == expected_root_hash
            } else {
                decoded.root.as_deref() == Some(ctx.root_display.as_str())
            };
            if decoded.v == CURSOR_VERSION
                && decoded.tool == "read_pack"
                && decoded.mode == "memory"
                && root_matches
            {
                start_candidate_index = decoded.next_candidate_index;
                entrypoint_done_from_cursor = decoded.entrypoint_done;
            }
        }
    }

    let candidates = collect_memory_file_candidates(&ctx.root);
    if candidates.is_empty() || start_candidate_index >= candidates.len() {
        return;
    }

    let mut last_idx: Option<usize> = None;
    for section in &result.sections {
        let Some(file) = read_pack_section_file(section) else {
            continue;
        };
        if let Some(idx) = candidates.iter().position(|candidate| candidate == file) {
            if idx >= start_candidate_index {
                last_idx = Some(last_idx.map_or(idx, |prev| prev.max(idx)));
            }
        }
    }
    let next_candidate_index = last_idx.map_or(start_candidate_index, |idx| idx + 1);
    if next_candidate_index >= candidates.len() {
        return;
    }

    // Avoid returning a cursor that will immediately yield an empty page.
    let has_more_payload = candidates
        .iter()
        .skip(next_candidate_index)
        .any(|rel| ctx.root.join(rel).is_file() && !is_disallowed_memory_file(rel));
    if !has_more_payload {
        return;
    }

    let entrypoint_file: Option<String> = result.sections.iter().find_map(|section| {
        let ReadPackSection::ProjectFacts { result } = section else {
            return None;
        };
        result
            .entry_points
            .iter()
            .find(|rel| ctx.root.join(*rel).is_file() && !is_disallowed_memory_file(rel))
            .cloned()
    });
    let entrypoint_in_sections = entrypoint_file.as_deref().is_some_and(|needle| {
        result
            .sections
            .iter()
            .filter_map(read_pack_section_file)
            .any(|file| file == needle)
    });
    let entrypoint_done = entrypoint_done_from_cursor || entrypoint_in_sections;

    let cursor = ReadPackMemoryCursorV1 {
        v: CURSOR_VERSION,
        tool: "read_pack".to_string(),
        mode: "memory".to_string(),
        root: Some(ctx.root_display.clone()),
        root_hash: Some(cursor_fingerprint(&ctx.root_display)),
        max_chars: Some(ctx.max_chars),
        response_mode: Some(response_mode),
        next_candidate_index,
        entrypoint_done,
    };
    if let Ok(token) = encode_cursor(&cursor) {
        result.next_cursor = Some(compact_cursor_alias(service, token).await);
    }
}

async fn repair_recall_cursor_after_trim(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    result: &mut ReadPackResult,
) {
    let (
        questions,
        topics,
        include_paths,
        exclude_paths,
        file_pattern,
        prefer_code,
        include_docs,
        allow_secrets,
    ) = if let Some(cursor) = trimmed_non_empty_str(request.cursor.as_deref()) {
        match decode_recall_cursor(service, cursor).await {
            Ok(decoded) => (
                decoded.questions,
                decoded.topics,
                decoded.include_paths,
                decoded.exclude_paths,
                decoded.file_pattern,
                decoded.prefer_code,
                decoded.include_docs,
                decoded.allow_secrets,
            ),
            Err(_) => return,
        }
    } else {
        (
            normalize_questions(request),
            normalize_topics(request),
            normalize_path_prefix_list(request.include_paths.as_ref()),
            normalize_path_prefix_list(request.exclude_paths.as_ref()),
            normalize_optional_pattern(request.file_pattern.as_deref()),
            request.prefer_code,
            request.include_docs,
            request.allow_secrets.unwrap_or(false),
        )
    };

    if questions.is_empty() {
        result.next_cursor = None;
        return;
    }

    let answered = result
        .sections
        .iter()
        .filter(|section| matches!(section, ReadPackSection::Recall { .. }))
        .count();
    if answered >= questions.len() {
        result.next_cursor = None;
        return;
    }

    let remaining_questions: Vec<String> = questions.into_iter().skip(answered).collect();
    if remaining_questions.is_empty() {
        result.next_cursor = None;
        return;
    }

    let cursor = ReadPackRecallCursorV1 {
        v: CURSOR_VERSION,
        tool: "read_pack".to_string(),
        mode: "recall".to_string(),
        root: Some(ctx.root_display.clone()),
        root_hash: Some(cursor_fingerprint(&ctx.root_display)),
        max_chars: Some(ctx.max_chars),
        response_mode: Some(response_mode),
        questions: remaining_questions,
        topics,
        include_paths,
        exclude_paths,
        file_pattern,
        prefer_code,
        include_docs,
        allow_secrets,
        next_question_index: 0,
    };

    if let Ok(token) = encode_cursor(&cursor) {
        if token.len() <= MAX_RECALL_INLINE_CURSOR_CHARS {
            result.next_cursor = Some(compact_cursor_alias(service, token).await);
            return;
        }
    }

    let stored_bytes = match serde_json::to_vec(&cursor) {
        Ok(bytes) => bytes,
        Err(_) => return,
    };
    let store_id = service.state.cursor_store_put(stored_bytes).await;
    let stored_cursor = ReadPackRecallCursorStoredV1 {
        v: CURSOR_VERSION,
        tool: "read_pack".to_string(),
        mode: "recall".to_string(),
        root: Some(ctx.root_display.clone()),
        root_hash: Some(cursor_fingerprint(&ctx.root_display)),
        max_chars: Some(ctx.max_chars),
        response_mode: Some(response_mode),
        store_id,
    };
    if let Ok(token) = encode_cursor(&stored_cursor) {
        result.next_cursor = Some(compact_cursor_alias(service, token).await);
    }
}

async fn repair_cursor_after_trim(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    intent: ReadPackIntent,
    response_mode: ResponseMode,
    result: &mut ReadPackResult,
) {
    match intent {
        ReadPackIntent::Memory => {
            repair_memory_cursor_after_trim(service, ctx, request, response_mode, result).await;
        }
        ReadPackIntent::Recall => {
            repair_recall_cursor_after_trim(service, ctx, request, response_mode, result).await;
        }
        _ => {}
    }
}

fn ensure_retry_action(
    result: &mut ReadPackResult,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    intent: ReadPackIntent,
) {
    if !result.budget.truncated || !result.next_actions.is_empty() {
        return;
    }

    let suggested_max_chars = ctx
        .max_chars
        .saturating_mul(2)
        .clamp(DEFAULT_MAX_CHARS, MAX_MAX_CHARS);

    let args = build_retry_args(ctx, request, intent, suggested_max_chars);
    result.next_actions.push(ReadPackNextAction {
        tool: "read_pack".to_string(),
        args,
        reason: "Increase max_chars to get a fuller read_pack payload.".to_string(),
    });
    let _ = finalize_read_pack_budget(result);
}

fn build_retry_args(
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    intent: ReadPackIntent,
    max_chars: usize,
) -> serde_json::Value {
    let mut args = serde_json::Map::new();
    args.insert(
        "path".to_string(),
        serde_json::Value::String(ctx.root_display.clone()),
    );
    args.insert(
        "intent".to_string(),
        serde_json::Value::String(intent_label(intent).to_string()),
    );
    args.insert(
        "max_chars".to_string(),
        serde_json::Value::Number(max_chars.into()),
    );

    if let Some(mode) = request.response_mode {
        args.insert(
            "response_mode".to_string(),
            serde_json::to_value(mode).unwrap_or(serde_json::Value::Null),
        );
    }

    if let Some(cursor) = trimmed_non_empty_str(request.cursor.as_deref()) {
        args.insert(
            "cursor".to_string(),
            serde_json::Value::String(cursor.to_string()),
        );
    }
    if let Some(timeout_ms) = request.timeout_ms {
        args.insert(
            "timeout_ms".to_string(),
            serde_json::Value::Number(timeout_ms.into()),
        );
    }

    match intent {
        ReadPackIntent::File => {
            if let Some(file) = trimmed_non_empty_str(request.file.as_deref()) {
                args.insert(
                    "file".to_string(),
                    serde_json::Value::String(file.to_string()),
                );
            }
            if let Some(start_line) = request.start_line {
                args.insert(
                    "start_line".to_string(),
                    serde_json::Value::Number(start_line.into()),
                );
            }
            if let Some(max_lines) = request.max_lines {
                args.insert(
                    "max_lines".to_string(),
                    serde_json::Value::Number(max_lines.into()),
                );
            }
        }
        ReadPackIntent::Grep => {
            if let Some(pattern) = trimmed_non_empty_str(request.pattern.as_deref()) {
                args.insert(
                    "pattern".to_string(),
                    serde_json::Value::String(pattern.to_string()),
                );
            }
            if let Some(file_pattern) = trimmed_non_empty_str(request.file_pattern.as_deref()) {
                args.insert(
                    "file_pattern".to_string(),
                    serde_json::Value::String(file_pattern.to_string()),
                );
            }
            if let Some(before) = request.before {
                args.insert(
                    "before".to_string(),
                    serde_json::Value::Number(before.into()),
                );
            }
            if let Some(after) = request.after {
                args.insert("after".to_string(), serde_json::Value::Number(after.into()));
            }
            if let Some(case_sensitive) = request.case_sensitive {
                args.insert(
                    "case_sensitive".to_string(),
                    serde_json::Value::Bool(case_sensitive),
                );
            }
        }
        ReadPackIntent::Query => {
            if let Some(query) = trimmed_non_empty_str(request.query.as_deref()) {
                args.insert(
                    "query".to_string(),
                    serde_json::Value::String(query.to_string()),
                );
            }
            if let Some(file_pattern) = trimmed_non_empty_str(request.file_pattern.as_deref()) {
                args.insert(
                    "file_pattern".to_string(),
                    serde_json::Value::String(file_pattern.to_string()),
                );
            }
            if let Some(include_paths) = request.include_paths.as_ref() {
                let include_paths: Vec<serde_json::Value> = include_paths
                    .iter()
                    .map(|p| p.trim())
                    .filter(|p| !p.is_empty())
                    .map(|p| serde_json::Value::String(p.to_string()))
                    .collect();
                if !include_paths.is_empty() {
                    args.insert(
                        "include_paths".to_string(),
                        serde_json::Value::Array(include_paths),
                    );
                }
            }
            if let Some(exclude_paths) = request.exclude_paths.as_ref() {
                let exclude_paths: Vec<serde_json::Value> = exclude_paths
                    .iter()
                    .map(|p| p.trim())
                    .filter(|p| !p.is_empty())
                    .map(|p| serde_json::Value::String(p.to_string()))
                    .collect();
                if !exclude_paths.is_empty() {
                    args.insert(
                        "exclude_paths".to_string(),
                        serde_json::Value::Array(exclude_paths),
                    );
                }
            }
            if let Some(prefer_code) = request.prefer_code {
                args.insert(
                    "prefer_code".to_string(),
                    serde_json::Value::Bool(prefer_code),
                );
            }
            if let Some(include_docs) = request.include_docs {
                args.insert(
                    "include_docs".to_string(),
                    serde_json::Value::Bool(include_docs),
                );
            }
        }
        ReadPackIntent::Recall => {
            if let Some(ask) = trimmed_non_empty_str(request.ask.as_deref()) {
                args.insert(
                    "ask".to_string(),
                    serde_json::Value::String(ask.to_string()),
                );
            }
            if let Some(questions) = request.questions.as_ref() {
                let questions: Vec<serde_json::Value> = questions
                    .iter()
                    .map(|q| q.trim())
                    .filter(|q| !q.is_empty())
                    .map(|q| serde_json::Value::String(q.to_string()))
                    .collect();
                if !questions.is_empty() {
                    args.insert("questions".to_string(), serde_json::Value::Array(questions));
                }
            }
            if let Some(topics) = request.topics.as_ref() {
                let topics: Vec<serde_json::Value> = topics
                    .iter()
                    .map(|t| t.trim())
                    .filter(|t| !t.is_empty())
                    .map(|t| serde_json::Value::String(t.to_string()))
                    .collect();
                if !topics.is_empty() {
                    args.insert("topics".to_string(), serde_json::Value::Array(topics));
                }
            }
            if let Some(file_pattern) = trimmed_non_empty_str(request.file_pattern.as_deref()) {
                args.insert(
                    "file_pattern".to_string(),
                    serde_json::Value::String(file_pattern.to_string()),
                );
            }
            if let Some(include_paths) = request.include_paths.as_ref() {
                let include_paths: Vec<serde_json::Value> = include_paths
                    .iter()
                    .map(|p| p.trim())
                    .filter(|p| !p.is_empty())
                    .map(|p| serde_json::Value::String(p.to_string()))
                    .collect();
                if !include_paths.is_empty() {
                    args.insert(
                        "include_paths".to_string(),
                        serde_json::Value::Array(include_paths),
                    );
                }
            }
            if let Some(exclude_paths) = request.exclude_paths.as_ref() {
                let exclude_paths: Vec<serde_json::Value> = exclude_paths
                    .iter()
                    .map(|p| p.trim())
                    .filter(|p| !p.is_empty())
                    .map(|p| serde_json::Value::String(p.to_string()))
                    .collect();
                if !exclude_paths.is_empty() {
                    args.insert(
                        "exclude_paths".to_string(),
                        serde_json::Value::Array(exclude_paths),
                    );
                }
            }
        }
        ReadPackIntent::Onboarding | ReadPackIntent::Memory | ReadPackIntent::Auto => {}
    }

    serde_json::Value::Object(args)
}

fn apply_meta_to_sections(sections: &mut [ReadPackSection]) {
    for section in sections {
        match section {
            ReadPackSection::ProjectFacts { .. } => {}
            ReadPackSection::ExternalMemory { .. } => {}
            ReadPackSection::Snippet { .. } => {}
            ReadPackSection::Recall { .. } => {}
            ReadPackSection::Overview { result } => {
                result.meta = ToolMeta::default();
            }
            ReadPackSection::FileSlice { result } => {
                result.meta = None;
            }
            ReadPackSection::GrepContext { result } => {
                result.meta = None;
            }
            ReadPackSection::RepoOnboardingPack { result } => {
                result.meta = ToolMeta::default();
            }
            ReadPackSection::ContextPack { .. } => {}
        }
    }
}

fn decode_file_slice_cursor(cursor: Option<&str>) -> ToolResult<Option<FileSliceCursorV1>> {
    let Some(cursor) = trimmed_non_empty_str(cursor) else {
        return Ok(None);
    };

    let decoded: FileSliceCursorV1 = decode_cursor(cursor)
        .map_err(|err| call_error("invalid_cursor", format!("Invalid cursor: {err}")))?;
    Ok(Some(decoded))
}

async fn handle_file_intent(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    sections: &mut Vec<ReadPackSection>,
    next_actions: &mut Vec<ReadPackNextAction>,
    next_cursor_out: &mut Option<String>,
) -> ToolResult<()> {
    let expanded_cursor = match trimmed_non_empty_str(request.cursor.as_deref()) {
        Some(cursor) => Some(
            expand_cursor_alias(service, cursor)
                .await
                .map_err(|message| call_error("invalid_cursor", message))?,
        ),
        None => None,
    };
    let cursor_payload = decode_file_slice_cursor(expanded_cursor.as_deref())?;
    if let Some(decoded) = cursor_payload.as_ref() {
        if decoded.v != CURSOR_VERSION || (decoded.tool != "cat" && decoded.tool != "file_slice") {
            return Err(call_error(
                "invalid_cursor",
                "Invalid cursor: wrong tool (expected cat)",
            ));
        }
        let expected_root_hash = cursor_fingerprint(&ctx.root_display);
        let expected_root_fingerprint = root_fingerprint(&ctx.root_display);
        if let Some(hash) = decoded.root_hash {
            if hash != expected_root_hash {
                return Err(invalid_cursor_with_meta_details(
                    "Invalid cursor: different root",
                    ToolMeta {
                        root_fingerprint: Some(expected_root_fingerprint),
                        ..ToolMeta::default()
                    },
                    json!({
                        "expected_root_fingerprint": expected_root_fingerprint,
                        "cursor_root_fingerprint": Some(hash),
                    }),
                ));
            }
        } else if decoded.root.as_deref() != Some(ctx.root_display.as_str()) {
            let cursor_root_fingerprint = decoded.root.as_deref().map(root_fingerprint);
            return Err(invalid_cursor_with_meta_details(
                "Invalid cursor: different root",
                ToolMeta {
                    root_fingerprint: Some(expected_root_fingerprint),
                    ..ToolMeta::default()
                },
                json!({
                    "expected_root_fingerprint": expected_root_fingerprint,
                    "cursor_root_fingerprint": cursor_root_fingerprint,
                }),
            ));
        }
    }

    let requested_file = trimmed_non_empty_str(request.file.as_deref()).map(ToString::to_string);
    if let (Some(decoded), Some(requested)) = (cursor_payload.as_ref(), requested_file.as_ref()) {
        if requested != &decoded.file {
            return Err(call_error(
                "invalid_cursor",
                format!(
                    "Invalid cursor: different file (cursor={}, request={})",
                    decoded.file, requested
                ),
            ));
        }
    }

    let file = requested_file.or_else(|| cursor_payload.as_ref().map(|c| c.file.clone()));
    let Some(file) = file else {
        return Err(call_error(
            "missing_field",
            "Error: file is required for intent=file",
        ));
    };

    let allow_secrets = request
        .allow_secrets
        .or_else(|| cursor_payload.as_ref().map(|c| c.allow_secrets))
        .unwrap_or(false);
    if !allow_secrets && is_disallowed_memory_file(&file) {
        return Err(call_error(
            "forbidden_file",
            "Refusing to read potential secret file via read_pack",
        ));
    }

    let max_lines = request
        .max_lines
        .or_else(|| cursor_payload.as_ref().map(|c| c.max_lines));

    let file_slice_max_chars = if let Some(decoded) = cursor_payload.as_ref() {
        if request.max_chars.is_some() {
            ctx.inner_max_chars
        } else {
            decoded.max_chars
        }
    } else {
        match response_mode {
            ResponseMode::Full => ctx.inner_max_chars,
            ResponseMode::Facts | ResponseMode::Minimal => {
                snippet_inner_max_chars(ctx.inner_max_chars)
            }
        }
    };
    let mut slice = compute_file_slice_result(
        &ctx.root,
        &ctx.root_display,
        &FileSliceRequest {
            path: None,
            file: Some(file.clone()),
            start_line: request.start_line,
            max_lines,
            max_chars: Some(file_slice_max_chars),
            format: None,
            response_mode: Some(response_mode),
            allow_secrets: Some(allow_secrets),
            cursor: expanded_cursor,
        },
    )
    .map_err(|err| {
        if err.trim_start().starts_with("Invalid cursor") {
            call_error("invalid_cursor", err)
        } else {
            call_error("invalid_request", err)
        }
    })?;

    if let Some(cursor) = slice.next_cursor.take() {
        let compact = compact_cursor_alias(service, cursor).await;
        slice.next_cursor = Some(compact.clone());
        *next_cursor_out = Some(compact);
    } else {
        *next_cursor_out = None;
    }

    if response_mode == ResponseMode::Full {
        if let Some(next_cursor) = slice.next_cursor.as_deref() {
            next_actions.push(ReadPackNextAction {
                tool: "read_pack".to_string(),
                args: json!({
                    "path": ctx.root_display.clone(),
                    "intent": "file",
                    "file": file,
                    "max_lines": slice.max_lines,
                    "max_chars": ctx.max_chars,
                    "cursor": next_cursor,
                }),
                reason: "Continue reading the next page of the file slice.".to_string(),
            });
        }
    }

    if response_mode == ResponseMode::Full {
        sections.push(ReadPackSection::FileSlice { result: slice });
    } else {
        let kind = if response_mode == ResponseMode::Minimal {
            None
        } else {
            Some(snippet_kind_for_path(&file))
        };
        sections.push(ReadPackSection::Snippet {
            result: ReadPackSnippet {
                file: slice.file.clone(),
                start_line: slice.start_line,
                end_line: slice.end_line,
                content: slice.content.clone(),
                kind,
                reason: Some(REASON_INTENT_FILE.to_string()),
                // Cursor is already returned at the top-level (`read_pack.next_cursor`).
                // Avoid duplicating it inside the snippet: under tight budgets it can evict payload.
                next_cursor: None,
            },
        });
    }
    Ok(())
}

fn decode_grep_cursor(cursor: Option<&str>) -> ToolResult<Option<GrepContextCursorV1>> {
    let Some(cursor) = trimmed_non_empty_str(cursor) else {
        return Ok(None);
    };

    let decoded: GrepContextCursorV1 = decode_cursor(cursor)
        .map_err(|err| call_error("invalid_cursor", format!("Invalid cursor: {err}")))?;
    Ok(Some(decoded))
}

fn validate_grep_cursor_tool_root(
    decoded: &GrepContextCursorV1,
    root_display: &str,
) -> ToolResult<()> {
    if decoded.v != CURSOR_VERSION || (decoded.tool != "rg" && decoded.tool != "grep_context") {
        return Err(call_error(
            "invalid_cursor",
            "Invalid cursor: wrong tool (expected rg)",
        ));
    }
    let expected_root_hash = cursor_fingerprint(root_display);
    let expected_root_fingerprint = root_fingerprint(root_display);
    if let Some(hash) = decoded.root_hash {
        if hash != expected_root_hash {
            return Err(invalid_cursor_with_meta_details(
                "Invalid cursor: different root",
                ToolMeta {
                    root_fingerprint: Some(expected_root_fingerprint),
                    ..ToolMeta::default()
                },
                json!({
                    "expected_root_fingerprint": expected_root_fingerprint,
                    "cursor_root_fingerprint": Some(hash),
                }),
            ));
        }
    } else if decoded.root.as_deref() != Some(root_display) {
        let cursor_root_fingerprint = decoded.root.as_deref().map(root_fingerprint);
        return Err(invalid_cursor_with_meta_details(
            "Invalid cursor: different root",
            ToolMeta {
                root_fingerprint: Some(expected_root_fingerprint),
                ..ToolMeta::default()
            },
            json!({
                "expected_root_fingerprint": expected_root_fingerprint,
                "cursor_root_fingerprint": cursor_root_fingerprint,
            }),
        ));
    }
    Ok(())
}

fn resolve_grep_pattern(
    request_pattern: Option<&str>,
    cursor_payload: Option<&GrepContextCursorV1>,
    root_display: &str,
) -> ToolResult<String> {
    if let Some(pattern) = trimmed_non_empty_str(request_pattern) {
        return Ok(pattern.to_string());
    }

    if let Some(decoded) = cursor_payload {
        validate_grep_cursor_tool_root(decoded, root_display)?;
        return Ok(decoded.pattern.clone());
    }

    Err(call_error(
        "missing_field",
        "Error: pattern is required for intent=grep",
    ))
}

struct GrepResumeCheck<'a> {
    pattern: &'a str,
    file: Option<&'a String>,
    file_pattern: Option<&'a String>,
    case_sensitive: bool,
    before: usize,
    after: usize,
    allow_secrets: bool,
}

fn resolve_grep_resume(
    cursor_payload: Option<&GrepContextCursorV1>,
    root_display: &str,
    check: &GrepResumeCheck<'_>,
) -> ToolResult<(Option<String>, usize)> {
    let Some(decoded) = cursor_payload else {
        return Ok((None, 1));
    };
    validate_grep_cursor_tool_root(decoded, root_display)?;

    if decoded.pattern != check.pattern {
        return Err(call_error(
            "invalid_cursor",
            "Invalid cursor: different pattern",
        ));
    }
    if decoded.file.as_ref() != check.file {
        return Err(call_error(
            "invalid_cursor",
            "Invalid cursor: different file",
        ));
    }
    if decoded.file_pattern.as_ref() != check.file_pattern {
        return Err(call_error(
            "invalid_cursor",
            "Invalid cursor: different file_pattern",
        ));
    }
    if decoded.case_sensitive != check.case_sensitive
        || decoded.before != check.before
        || decoded.after != check.after
    {
        return Err(call_error(
            "invalid_cursor",
            "Invalid cursor: different search options",
        ));
    }
    if decoded.allow_secrets != check.allow_secrets {
        return Err(call_error(
            "invalid_cursor",
            "Invalid cursor: different allow_secrets",
        ));
    }

    Ok((
        Some(decoded.resume_file.clone()),
        decoded.resume_line.max(1),
    ))
}

async fn handle_grep_intent(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    sections: &mut Vec<ReadPackSection>,
    next_actions: &mut Vec<ReadPackNextAction>,
    next_cursor_out: &mut Option<String>,
) -> ToolResult<()> {
    let expanded_cursor = match trimmed_non_empty_str(request.cursor.as_deref()) {
        Some(cursor) => Some(
            expand_cursor_alias(service, cursor)
                .await
                .map_err(|message| call_error("invalid_cursor", message))?,
        ),
        None => None,
    };

    let cursor_payload = decode_grep_cursor(expanded_cursor.as_deref())?;
    let pattern = resolve_grep_pattern(
        request.pattern.as_deref(),
        cursor_payload.as_ref(),
        &ctx.root_display,
    )?;

    let case_sensitive = request
        .case_sensitive
        .or_else(|| cursor_payload.as_ref().map(|c| c.case_sensitive))
        .unwrap_or(true);
    let regex = RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|err| call_error("invalid_request", format!("Invalid regex: {err}")))?;

    let before = request
        .before
        .or_else(|| cursor_payload.as_ref().map(|c| c.before))
        .unwrap_or(DEFAULT_GREP_CONTEXT)
        .clamp(0, 5_000);
    let after = request
        .after
        .or_else(|| cursor_payload.as_ref().map(|c| c.after))
        .unwrap_or(DEFAULT_GREP_CONTEXT)
        .clamp(0, 5_000);

    let normalized_file = trimmed_non_empty_str(request.file.as_deref())
        .map(str::to_string)
        .or_else(|| cursor_payload.as_ref().and_then(|c| c.file.clone()));
    let normalized_file_pattern = trimmed_non_empty_str(request.file_pattern.as_deref())
        .map(str::to_string)
        .or_else(|| cursor_payload.as_ref().and_then(|c| c.file_pattern.clone()));

    let allow_secrets = request
        .allow_secrets
        .or_else(|| cursor_payload.as_ref().map(|c| c.allow_secrets))
        .unwrap_or(false);
    if !allow_secrets {
        if let Some(file) = normalized_file.as_deref() {
            if is_disallowed_memory_file(file) {
                return Err(call_error(
                    "forbidden_file",
                    "Refusing to search potential secret file via read_pack",
                ));
            }
        }
    }

    let resume_check = GrepResumeCheck {
        pattern: pattern.as_str(),
        file: normalized_file.as_ref(),
        file_pattern: normalized_file_pattern.as_ref(),
        case_sensitive,
        before,
        after,
        allow_secrets,
    };
    let (resume_file, resume_line) =
        resolve_grep_resume(cursor_payload.as_ref(), &ctx.root_display, &resume_check)?;

    let grep_max_chars = (ctx.inner_max_chars / 2).max(200);
    let grep_content_max_chars =
        super::router::grep_context::grep_context_content_budget(grep_max_chars, response_mode);
    let max_hunks = (grep_max_chars / 200).clamp(1, MAX_GREP_HUNKS);
    let format = match response_mode {
        ResponseMode::Full => None,
        ResponseMode::Facts | ResponseMode::Minimal => Some(ContentFormat::Plain),
    };
    let grep_request = GrepContextRequest {
        path: None,
        pattern: Some(pattern.clone()),
        literal: Some(false),
        file: normalized_file,
        file_pattern: normalized_file_pattern,
        context: None,
        before: Some(before),
        after: Some(after),
        max_matches: Some(MAX_GREP_MATCHES),
        max_hunks: Some(max_hunks),
        max_chars: Some(grep_max_chars),
        case_sensitive: Some(case_sensitive),
        format,
        response_mode: Some(response_mode),
        allow_secrets: Some(allow_secrets),
        cursor: None,
    };

    let mut result = compute_grep_context_result(
        &ctx.root,
        &ctx.root_display,
        &grep_request,
        &regex,
        GrepContextComputeOptions {
            case_sensitive,
            before,
            after,
            max_matches: MAX_GREP_MATCHES,
            max_hunks,
            max_chars: grep_max_chars,
            content_max_chars: grep_content_max_chars,
            resume_file: resume_file.as_deref(),
            resume_line,
        },
    )
    .await
    .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;

    if let Some(cursor) = result.next_cursor.take() {
        let compact = compact_cursor_alias(service, cursor).await;
        result.next_cursor = Some(compact.clone());
        *next_cursor_out = Some(compact);
    } else {
        *next_cursor_out = None;
    }

    if response_mode == ResponseMode::Full {
        if let Some(next_cursor) = result.next_cursor.as_deref() {
            let GrepContextRequest {
                file, file_pattern, ..
            } = grep_request;
            next_actions.push(ReadPackNextAction {
                tool: "read_pack".to_string(),
                args: json!({
                    "path": ctx.root_display.clone(),
                    "intent": "grep",
                    "pattern": pattern,
                    "file": file,
                    "file_pattern": file_pattern,
                    "before": before,
                    "after": after,
                    "case_sensitive": case_sensitive,
                    "max_chars": ctx.max_chars,
                    "cursor": next_cursor,
                }),
                reason: "Continue rg pagination (next page of hunks).".to_string(),
            });
        }
    }

    if response_mode == ResponseMode::Full {
        sections.push(ReadPackSection::GrepContext { result });
    } else {
        for hunk in result.hunks.iter().take(3) {
            let kind = if response_mode == ResponseMode::Minimal {
                None
            } else {
                Some(snippet_kind_for_path(&hunk.file))
            };
            sections.push(ReadPackSection::Snippet {
                result: ReadPackSnippet {
                    file: hunk.file.clone(),
                    start_line: hunk.start_line,
                    end_line: hunk.end_line,
                    content: hunk.content.clone(),
                    kind,
                    reason: Some(REASON_NEEDLE_GREP_HUNK.to_string()),
                    next_cursor: None,
                },
            });
        }
    }
    Ok(())
}

async fn handle_query_intent(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    policy: QueryIntentPolicy,
    sections: &mut Vec<ReadPackSection>,
) -> ToolResult<()> {
    let query = trimmed_non_empty_str(request.query.as_deref())
        .unwrap_or("")
        .to_string();
    if query.is_empty() {
        return Err(call_error(
            "missing_field",
            "Error: query is required for intent=query",
        ));
    }

    let mut insert_at = sections
        .iter()
        .position(|section| matches!(section, ReadPackSection::ProjectFacts { .. }))
        .map(|idx| idx + 1)
        .unwrap_or(0);
    for memory in
        crate::tools::external_memory::overlays_for_query(&ctx.root, &query, response_mode).await
    {
        sections.insert(
            insert_at,
            ReadPackSection::ExternalMemory { result: memory },
        );
        insert_at = insert_at.saturating_add(1);
    }

    let tool_result = super::router::context_pack::context_pack(
        service,
        ContextPackRequest {
            path: Some(ctx.root_display.clone()),
            query,
            language: None,
            strategy: None,
            limit: None,
            max_chars: Some(ctx.inner_max_chars),
            include_paths: request.include_paths.clone(),
            exclude_paths: request.exclude_paths.clone(),
            file_pattern: request.file_pattern.clone(),
            max_related_per_primary: None,
            include_docs: request.include_docs,
            prefer_code: request.prefer_code,
            related_mode: None,
            response_mode: request.response_mode,
            trace: Some(false),
            auto_index: None,
            auto_index_budget_ms: None,
        },
    )
    .await
    .map_err(|err| call_error("internal", format!("Error: {err}")))?;

    if tool_result.is_error == Some(true) {
        return Err(tool_result);
    }

    let mut value: serde_json::Value = tool_result.structured_content.clone().ok_or_else(|| {
        call_error(
            "internal",
            "Error: context_pack returned no structured_content",
        )
    })?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("meta");
        if response_mode != ResponseMode::Full {
            obj.remove("next_actions");
        }
    }

    if response_mode == ResponseMode::Full {
        sections.push(ReadPackSection::ContextPack { result: value });
        return Ok(());
    }

    let snippet_max_chars = (ctx.inner_max_chars / 4)
        .clamp(200, 4_000)
        .min(ctx.inner_max_chars);
    let mut added = 0usize;

    let items = value
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    for role in ["primary", "related"] {
        for item in &items {
            if added >= 5 {
                break;
            }
            if item.get("role").and_then(Value::as_str) != Some(role) {
                continue;
            }
            let Some(file) = item.get("file").and_then(Value::as_str) else {
                continue;
            };
            if !policy.allow_secrets && is_disallowed_memory_file(file) {
                continue;
            }
            let Some(content) = item.get("content").and_then(Value::as_str) else {
                continue;
            };
            let start_line = item.get("start_line").and_then(Value::as_u64).unwrap_or(1) as usize;
            let end_line = item
                .get("end_line")
                .and_then(Value::as_u64)
                .unwrap_or(start_line as u64) as usize;
            let kind = if response_mode == ResponseMode::Minimal {
                None
            } else {
                Some(snippet_kind_for_path(file))
            };
            let reason = match role {
                "primary" => Some(REASON_HALO_CONTEXT_PACK_PRIMARY.to_string()),
                _ => Some(REASON_HALO_CONTEXT_PACK_RELATED.to_string()),
            };
            sections.push(ReadPackSection::Snippet {
                result: ReadPackSnippet {
                    file: file.to_string(),
                    start_line,
                    end_line,
                    content: trim_chars(content, snippet_max_chars),
                    kind,
                    reason,
                    next_cursor: None,
                },
            });
            added += 1;
        }
    }

    if added == 0 {
        // Fallback: emit the raw context_pack JSON (already stripped) so the agent can see "no hits".
        sections.push(ReadPackSection::ContextPack { result: value });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct QueryIntentPolicy {
    allow_secrets: bool,
}

async fn handle_onboarding_intent(
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    facts: &ProjectFactsResult,
    sections: &mut Vec<ReadPackSection>,
) -> ToolResult<()> {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum OnboardingTopic {
        Tests,
        Run,
        Build,
        Install,
        CI,
        Structure,
        Unknown,
    }

    fn onboarding_prompt(request: &ReadPackRequest) -> String {
        let mut parts = Vec::new();
        if let Some(text) = trimmed_non_empty_str(request.ask.as_deref()) {
            parts.push(text.to_string());
        }
        if let Some(text) = trimmed_non_empty_str(request.query.as_deref()) {
            parts.push(text.to_string());
        }
        if let Some(questions) = request.questions.as_ref() {
            for q in questions {
                if let Some(text) = trimmed_non_empty_str(Some(q)) {
                    parts.push(text.to_string());
                }
            }
        }
        parts.join("\n")
    }

    fn classify_onboarding_topic(prompt: &str) -> OnboardingTopic {
        let lower = prompt.trim().to_ascii_lowercase();
        if lower.contains("test")
            || lower.contains("pytest")
            || lower.contains("тест")
            || lower.contains("прогнать тест")
        {
            return OnboardingTopic::Tests;
        }
        if lower.contains("how to run")
            || lower.contains("run ")
            || lower.contains("start")
            || lower.contains("запустить")
            || lower.contains("запуск")
        {
            return OnboardingTopic::Run;
        }
        if lower.contains("build")
            || lower.contains("compile")
            || lower.contains("собрать")
            || lower.contains("сборка")
        {
            return OnboardingTopic::Build;
        }
        if lower.contains("install")
            || lower.contains("setup")
            || lower.contains("dependencies")
            || lower.contains("deps")
            || lower.contains("установ")
        {
            return OnboardingTopic::Install;
        }
        if lower.contains("ci")
            || lower.contains("github actions")
            || lower.contains("pipeline")
            || lower.contains("workflows")
        {
            return OnboardingTopic::CI;
        }
        if lower.contains("architecture")
            || lower.contains("entry point")
            || lower.contains("repo structure")
            || lower.contains("project structure")
            || lower.contains("архитектур")
            || lower.contains("точка входа")
            || lower.contains("структура")
        {
            return OnboardingTopic::Structure;
        }
        OnboardingTopic::Unknown
    }

    fn command_grep_pattern(topic: OnboardingTopic, facts: &ProjectFactsResult) -> Option<String> {
        fn has_token(values: &[String], needle: &str) -> bool {
            let needle = needle.trim().to_ascii_lowercase();
            if needle.is_empty() {
                return false;
            }
            values
                .iter()
                .any(|v| v.to_ascii_lowercase().contains(&needle))
        }

        let has_rust =
            has_token(&facts.ecosystems, "rust") || has_token(&facts.build_tools, "cargo");
        let has_node = has_token(&facts.ecosystems, "node")
            || has_token(&facts.build_tools, "npm")
            || has_token(&facts.build_tools, "pnpm")
            || has_token(&facts.build_tools, "yarn");
        let has_python =
            has_token(&facts.ecosystems, "python") || has_token(&facts.build_tools, "pip");
        let has_go = has_token(&facts.ecosystems, "go") || has_token(&facts.build_tools, "go");
        let has_java =
            has_token(&facts.build_tools, "maven") || has_token(&facts.build_tools, "gradle");

        match topic {
            OnboardingTopic::Tests => {
                let mut patterns = Vec::new();
                if has_rust {
                    patterns.push(r"(?i)\bcargo\s+test\b|\bcargo\s+clippy\b|\bcargo\s+fmt\b");
                }
                if has_node {
                    patterns.push(r"(?i)\bnpm\s+test\b|\bpnpm\s+test\b|\byarn\s+test\b");
                }
                if has_python {
                    patterns.push(r"(?i)\bpytest\b|\bpython\s+-m\s+pytest\b|\btox\b");
                }
                if has_go {
                    patterns.push(r"(?i)\bgo\s+test\b");
                }
                if has_java {
                    patterns.push(r"(?i)\bmvn\s+test\b|\bgradle\b.*\btest\b");
                }
                if patterns.is_empty() {
                    patterns.push(r"(?i)\bcargo\s+test\b|\bpytest\b|\bgo\s+test\b|\bnpm\s+test\b");
                }
                Some(patterns.join("|"))
            }
            OnboardingTopic::Run => {
                let mut patterns = Vec::new();
                if has_rust {
                    patterns.push(r"(?i)\bcargo\s+run\b");
                }
                if has_node {
                    patterns.push(
                        r"(?i)\bnpm\s+(run\s+)?start\b|\bpnpm\s+(run\s+)?start\b|\byarn\s+start\b",
                    );
                }
                if has_go {
                    patterns.push(r"(?i)\bgo\s+run\b");
                }
                patterns.push(r"(?i)\bdocker\s+compose\s+up\b");
                patterns.push(r"(?i)\bmake\s+run\b|\bjust\s+run\b");
                Some(patterns.join("|"))
            }
            OnboardingTopic::Build => {
                let mut patterns = Vec::new();
                if has_rust {
                    patterns.push(r"(?i)\bcargo\s+build\b");
                }
                if has_node {
                    patterns
                        .push(r"(?i)\bnpm\s+run\s+build\b|\bpnpm\s+run\s+build\b|\byarn\s+build\b");
                }
                if has_go {
                    patterns.push(r"(?i)\bgo\s+build\b");
                }
                if has_java {
                    patterns.push(r"(?i)\bmvn\s+package\b|\bgradle\b.*\bbuild\b");
                }
                patterns.push(r"(?i)\bmake\s+build\b|\bjust\s+build\b");
                Some(patterns.join("|"))
            }
            OnboardingTopic::Install => {
                let mut patterns = Vec::new();
                if has_rust {
                    patterns.push(r"(?i)\bcargo\s+install\b");
                }
                if has_node {
                    patterns.push(r"(?i)\bnpm\s+install\b|\bpnpm\s+install\b|\byarn\s+install\b");
                }
                if has_python {
                    patterns.push(r"(?i)\bpip\s+install\b|\bpoetry\s+install\b");
                }
                if has_go {
                    patterns.push(r"(?i)\bgo\s+mod\s+tidy\b|\bgo\s+get\b");
                }
                patterns.push(r"(?i)\bbundle\s+install\b");
                Some(patterns.join("|"))
            }
            OnboardingTopic::CI => {
                Some(r"(?i)\.github/workflows|github actions|\bci\b".to_string())
            }
            OnboardingTopic::Structure | OnboardingTopic::Unknown => None,
        }
    }

    fn onboarding_doc_candidates(topic: OnboardingTopic) -> Vec<&'static str> {
        let mut out = vec!["AGENTS.md", "README.md", "docs/QUICK_START.md"];
        match topic {
            OnboardingTopic::Tests => {
                out.extend([
                    "CONTRIBUTING.md",
                    "USAGE_EXAMPLES.md",
                    "scripts/validate_quality.sh",
                    "scripts/validate_contracts.sh",
                ]);
            }
            OnboardingTopic::Run => {
                out.extend([
                    "USAGE_EXAMPLES.md",
                    "docs/README.md",
                    "compose.yml",
                    "docker-compose.yml",
                ]);
            }
            OnboardingTopic::Build => {
                out.extend(["USAGE_EXAMPLES.md", "Makefile", "Justfile"]);
            }
            OnboardingTopic::Install => {
                out.extend(["CONTRIBUTING.md", "docs/README.md"]);
            }
            OnboardingTopic::CI => {
                out.extend([".github/workflows/ci.yml", "docs/README.md"]);
            }
            OnboardingTopic::Structure => {
                out.extend(["PHILOSOPHY.md", "docs/README.md"]);
            }
            OnboardingTopic::Unknown => {
                out.extend(["PHILOSOPHY.md", "docs/README.md"]);
            }
        }
        out
    }

    fn onboarding_docs_budget(
        ctx: &ReadPackContext,
        response_mode: ResponseMode,
    ) -> (usize, usize, usize) {
        let inner = ctx.inner_max_chars.max(1);
        let mut docs_limit = if inner <= 1_400 {
            1usize
        } else if inner <= 3_000 {
            2usize
        } else if inner <= 6_000 {
            3usize
        } else {
            4usize
        };
        if response_mode == ResponseMode::Minimal {
            docs_limit = docs_limit.min(2);
        }

        // Keep per-doc slices small and deterministic so tiny budgets still return at least one
        // useful anchor.
        let doc_max_lines = if inner <= 2_000 { 80 } else { 200 };
        let doc_max_chars = (inner / (docs_limit + 2)).clamp(240, 2_000);
        (docs_limit, doc_max_lines, doc_max_chars)
    }

    let prompt = onboarding_prompt(request);
    let topic = classify_onboarding_topic(&prompt);

    if response_mode == ResponseMode::Full {
        let onboarding_request = RepoOnboardingPackRequest {
            path: Some(ctx.root_display.clone()),
            map_depth: None,
            map_limit: None,
            doc_paths: None,
            docs_limit: None,
            doc_max_lines: None,
            doc_max_chars: None,
            max_chars: Some(ctx.inner_max_chars),
            response_mode: None,
            auto_index: None,
            auto_index_budget_ms: None,
        };

        let pack =
            compute_repo_onboarding_pack_result(&ctx.root, &ctx.root_display, &onboarding_request)
                .await
                .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;
        sections.push(ReadPackSection::RepoOnboardingPack {
            result: Box::new(pack),
        });
        return Ok(());
    }

    // Facts/minimal mode is `.context`-first. Avoid computing a full repo_onboarding_pack (map +
    // next_actions) just to emit a couple of doc snippets: produce a cheap, deterministic set of
    // anchors and (when the prompt is about running/building/testing) add a "command needle" via
    // bounded grep.
    let (mut docs_limit, doc_max_lines, doc_max_chars) = onboarding_docs_budget(ctx, response_mode);

    let mut found_command = false;
    if let Some(pattern) = command_grep_pattern(topic, facts) {
        let grep_max_chars = (ctx.inner_max_chars / 3).clamp(240, 1_200);
        let grep_content_max_chars =
            super::router::grep_context::grep_context_content_budget(grep_max_chars, response_mode);
        let max_hunks = 1usize;
        let before = 4usize;
        let after = 4usize;
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(true)
            .build()
            .map_err(|err| call_error("invalid_request", format!("Invalid regex: {err}")))?;

        // 1) Cheap + precise: scan a small shortlist of high-signal "ops" files first.
        let probe_limit = if ctx.inner_max_chars <= 2_000 {
            6usize
        } else {
            10usize
        };
        for rel in collect_ops_file_candidates(&ctx.root)
            .into_iter()
            .take(probe_limit)
        {
            let grep_request = GrepContextRequest {
                path: None,
                pattern: Some(pattern.clone()),
                literal: Some(false),
                file: Some(rel),
                file_pattern: None,
                context: None,
                before: Some(before),
                after: Some(after),
                max_matches: Some(2_000),
                max_hunks: Some(max_hunks),
                max_chars: Some(grep_max_chars),
                case_sensitive: Some(false),
                format: Some(ContentFormat::Plain),
                response_mode: Some(response_mode),
                allow_secrets: Some(false),
                cursor: None,
            };

            let result = compute_grep_context_result(
                &ctx.root,
                &ctx.root_display,
                &grep_request,
                &regex,
                GrepContextComputeOptions {
                    case_sensitive: false,
                    before,
                    after,
                    max_matches: 2_000,
                    max_hunks,
                    max_chars: grep_max_chars,
                    content_max_chars: grep_content_max_chars,
                    resume_file: None,
                    resume_line: 1,
                },
            )
            .await
            .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;

            if let Some(hunk) = result.hunks.first() {
                let kind = if response_mode == ResponseMode::Minimal {
                    None
                } else {
                    Some(snippet_kind_for_path(&hunk.file))
                };
                sections.push(ReadPackSection::Snippet {
                    result: ReadPackSnippet {
                        file: hunk.file.clone(),
                        start_line: hunk.start_line,
                        end_line: hunk.end_line,
                        content: hunk.content.clone(),
                        kind,
                        reason: Some(REASON_NEEDLE_GREP_HUNK.to_string()),
                        next_cursor: None,
                    },
                });
                found_command = true;
                break;
            }
        }

        // 2) Fallback: one bounded repo-wide scan if the shortlist didn't hit anything.
        if !found_command {
            let grep_request = GrepContextRequest {
                path: None,
                pattern: Some(pattern),
                literal: Some(false),
                file: None,
                file_pattern: None,
                context: None,
                before: Some(before),
                after: Some(after),
                max_matches: Some(2_000),
                max_hunks: Some(max_hunks),
                max_chars: Some(grep_max_chars),
                case_sensitive: Some(false),
                format: Some(ContentFormat::Plain),
                response_mode: Some(response_mode),
                allow_secrets: Some(false),
                cursor: None,
            };

            let result = compute_grep_context_result(
                &ctx.root,
                &ctx.root_display,
                &grep_request,
                &regex,
                GrepContextComputeOptions {
                    case_sensitive: false,
                    before,
                    after,
                    max_matches: 2_000,
                    max_hunks,
                    max_chars: grep_max_chars,
                    content_max_chars: grep_content_max_chars,
                    resume_file: None,
                    resume_line: 1,
                },
            )
            .await
            .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;

            if let Some(hunk) = result.hunks.first() {
                let kind = if response_mode == ResponseMode::Minimal {
                    None
                } else {
                    Some(snippet_kind_for_path(&hunk.file))
                };
                sections.push(ReadPackSection::Snippet {
                    result: ReadPackSnippet {
                        file: hunk.file.clone(),
                        start_line: hunk.start_line,
                        end_line: hunk.end_line,
                        content: hunk.content.clone(),
                        kind,
                        reason: Some(REASON_NEEDLE_GREP_HUNK.to_string()),
                        next_cursor: None,
                    },
                });
                found_command = true;
            }
        }
    }

    if found_command {
        // Noise governor: if we already surfaced an actionable command, cap anchors aggressively.
        docs_limit = docs_limit.saturating_sub(1).max(1);
    }

    let mut seen = HashSet::new();
    let mut added = 0usize;
    for rel in onboarding_doc_candidates(topic) {
        if added >= docs_limit {
            break;
        }
        if !seen.insert(rel) {
            continue;
        }
        let Ok(slice) =
            compute_onboarding_doc_slice(&ctx.root, rel, 1, doc_max_lines, doc_max_chars)
        else {
            continue;
        };
        let kind = if response_mode == ResponseMode::Minimal {
            None
        } else {
            Some(snippet_kind_for_path(&slice.file))
        };
        sections.push(ReadPackSection::Snippet {
            result: ReadPackSnippet {
                file: slice.file,
                start_line: slice.start_line,
                end_line: slice.end_line,
                content: slice.content,
                kind,
                reason: Some(REASON_ANCHOR_DOC.to_string()),
                next_cursor: None,
            },
        });
        added += 1;
    }

    if added == 0 {
        // Fallback: preserve the old behavior (structured pack conversion) so non-doc repos
        // still return something instead of an empty onboarding.
        let onboarding_request = RepoOnboardingPackRequest {
            path: Some(ctx.root_display.clone()),
            map_depth: None,
            map_limit: None,
            doc_paths: None,
            docs_limit: Some(docs_limit),
            doc_max_lines: Some(doc_max_lines),
            doc_max_chars: Some(doc_max_chars),
            max_chars: Some(ctx.inner_max_chars),
            response_mode: None,
            auto_index: None,
            auto_index_budget_ms: None,
        };
        let mut pack =
            compute_repo_onboarding_pack_result(&ctx.root, &ctx.root_display, &onboarding_request)
                .await
                .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;
        pack.next_actions.clear();
        pack.map.next_actions = None;
        for doc in &mut pack.docs {
            doc.next_actions = None;
        }
        if response_mode == ResponseMode::Minimal {
            pack.meta.index_state = None;
            pack.map.meta = None;
            for doc in &mut pack.docs {
                doc.meta = None;
            }
        }

        for slice in pack.docs {
            let kind = if response_mode == ResponseMode::Minimal {
                None
            } else {
                Some(snippet_kind_for_path(&slice.file))
            };
            sections.push(ReadPackSection::Snippet {
                result: ReadPackSnippet {
                    file: slice.file,
                    start_line: slice.start_line,
                    end_line: slice.end_line,
                    content: slice.content,
                    kind,
                    reason: Some(REASON_ANCHOR_DOC.to_string()),
                    next_cursor: None,
                },
            });
        }
    }
    Ok(())
}

async fn handle_memory_intent(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    sections: &mut Vec<ReadPackSection>,
    next_actions: &mut Vec<ReadPackNextAction>,
    next_cursor_out: &mut Option<String>,
) -> ToolResult<()> {
    if response_mode == ResponseMode::Full {
        // Memory-pack default UX is "native-fast": avoid graph/index-heavy work unless we already
        // have a fresh semantic index (otherwise overview would trigger reindex work).
        let meta = service.tool_meta(&ctx.root).await;
        let has_fresh_index = meta
            .index_state
            .as_ref()
            .is_some_and(|state| state.index.exists && !state.stale);

        if has_fresh_index {
            let overview_request = OverviewRequest {
                path: Some(ctx.root_display.clone()),
                language: None,
                response_mode: None,
                auto_index: None,
                auto_index_budget_ms: None,
            };

            if let Ok(tool_result) =
                super::router::overview::overview(service, overview_request).await
            {
                if tool_result.is_error != Some(true) {
                    if let Some(value) = tool_result.structured_content.clone() {
                        if let Ok(overview) = serde_json::from_value::<OverviewResult>(value) {
                            sections.push(ReadPackSection::Overview { result: overview });
                        }
                    }
                }
            }
        }
    }

    // Include recent Codex CLI worklog context (project-scoped, bounded, deduped) on the initial
    // memory pack entry. Cursor continuations should stay payload-focused and avoid repeating
    // overlays.
    if trimmed_non_empty_str(request.cursor.as_deref()).is_none() {
        let overlays =
            crate::tools::external_memory::overlays_recent(&ctx.root, response_mode).await;
        if !overlays.is_empty() {
            let mut insert_at = sections
                .iter()
                .position(|section| matches!(section, ReadPackSection::ProjectFacts { .. }))
                .map(|idx| idx + 1)
                .unwrap_or(0);
            for memory in overlays {
                sections.insert(
                    insert_at,
                    ReadPackSection::ExternalMemory { result: memory },
                );
                insert_at = insert_at.saturating_add(1);
            }
        }
    }

    let mut start_candidate_index = 0usize;
    let mut entrypoint_done = false;
    if let Some(cursor) = trimmed_non_empty_str(request.cursor.as_deref()) {
        let overrides = request.file.is_some()
            || request.pattern.is_some()
            || request.query.is_some()
            || request.ask.is_some()
            || request.questions.is_some()
            || request.topics.is_some()
            || request.file_pattern.is_some()
            || request.include_paths.is_some()
            || request.exclude_paths.is_some()
            || request.before.is_some()
            || request.after.is_some()
            || request.case_sensitive.is_some()
            || request.start_line.is_some()
            || request.prefer_code.is_some()
            || request.include_docs.is_some();
        if overrides {
            return Err(call_error(
                "invalid_cursor",
                "Cursor continuation does not allow overriding memory parameters",
            ));
        }

        let decoded: ReadPackMemoryCursorV1 = decode_cursor(cursor)
            .map_err(|err| call_error("invalid_cursor", format!("Invalid cursor: {err}")))?;
        if decoded.v != CURSOR_VERSION || decoded.tool != "read_pack" || decoded.mode != "memory" {
            return Err(call_error("invalid_cursor", "Invalid cursor: wrong tool"));
        }

        let expected_root_hash = cursor_fingerprint(&ctx.root_display);
        let expected_root_fingerprint = root_fingerprint(&ctx.root_display);
        if let Some(hash) = decoded.root_hash {
            if hash != expected_root_hash {
                return Err(invalid_cursor_with_meta_details(
                    "Invalid cursor: different root",
                    ToolMeta {
                        root_fingerprint: Some(expected_root_fingerprint),
                        ..ToolMeta::default()
                    },
                    json!({
                        "expected_root_fingerprint": expected_root_fingerprint,
                        "cursor_root_fingerprint": Some(hash),
                    }),
                ));
            }
        } else if decoded.root.as_deref() != Some(ctx.root_display.as_str()) {
            let cursor_root_fingerprint = decoded.root.as_deref().map(root_fingerprint);
            return Err(invalid_cursor_with_meta_details(
                "Invalid cursor: different root",
                ToolMeta {
                    root_fingerprint: Some(expected_root_fingerprint),
                    ..ToolMeta::default()
                },
                json!({
                    "expected_root_fingerprint": expected_root_fingerprint,
                    "cursor_root_fingerprint": cursor_root_fingerprint,
                }),
            ));
        }

        start_candidate_index = decoded.next_candidate_index;
        entrypoint_done = decoded.entrypoint_done;
    }

    let doc_max_lines = 180usize;

    let entrypoint_file: Option<String> = sections.iter().find_map(|section| {
        let ReadPackSection::ProjectFacts { result } = section else {
            return None;
        };
        result
            .entry_points
            .iter()
            .filter(|rel| ctx.root.join(*rel).is_file() && !is_disallowed_memory_file(rel))
            .max_by(|a, b| {
                entrypoint_candidate_score(b)
                    .cmp(&entrypoint_candidate_score(a))
                    .then_with(|| a.cmp(b))
            })
            .cloned()
    });

    // Budget the memory-pack like "native recall":
    // - start with stable facts
    // - show a few high-signal docs/config snippets
    // - if possible, include one entrypoint snippet (where execution starts)
    let wants_entrypoint = entrypoint_file.is_some() && ctx.inner_max_chars >= 1_200;
    let entry_reserved_chars = if wants_entrypoint {
        (ctx.inner_max_chars / 8)
            .clamp(240, 3_000)
            .min(ctx.inner_max_chars.saturating_sub(200))
    } else {
        0
    };

    // "Focus file" (microscope): if the session root was set using a file path, surface that file
    // once on the initial memory pack page. This makes the tool feel closer to a "native memory":
    // the agent sees both stable repo anchors (docs/config) and the current working file.
    //
    // Keep it low-noise:
    // - only on the first page (no cursor),
    // - never for secret paths,
    // - reserve a small, deterministic slice of the payload budget.
    let focus_file = if trimmed_non_empty_str(request.cursor.as_deref()).is_none() {
        service.session.lock().await.focus_file()
    } else {
        None
    }
    .filter(|rel| ctx.root.join(rel).is_file() && !is_disallowed_memory_file(rel));
    let wants_focus_file = focus_file.is_some() && ctx.inner_max_chars >= 1_200;
    let focus_reserved_chars = if wants_focus_file {
        (ctx.inner_max_chars / 10)
            .clamp(200, 1_500)
            .min(ctx.inner_max_chars.saturating_sub(200))
    } else {
        0
    };

    let docs_budget_chars = ctx
        .inner_max_chars
        .saturating_sub(entry_reserved_chars)
        .saturating_sub(focus_reserved_chars);

    // Budgeting heuristic (agent-native):
    // - under tight budgets, prefer fewer, larger snippets (more useful than many tiny 200-char peeks)
    // - under larger budgets, expand up to a small cap to keep "memory pack" dense but broad
    //
    // The target size is intentionally coarse and deterministic: it keeps behavior stable across
    // runs and projects, while still letting callers steer results by adjusting `max_chars`.
    const MEMORY_DOC_TARGET_CHARS: usize = 800;
    let docs_limit = ((docs_budget_chars.saturating_add(MEMORY_DOC_TARGET_CHARS - 1))
        / MEMORY_DOC_TARGET_CHARS)
        .clamp(1, 6)
        .min(DEFAULT_MEMORY_FILE_CANDIDATES.len());
    let mut doc_max_chars = (docs_budget_chars / docs_limit.max(1))
        .clamp(160, 6_000)
        .min(ctx.inner_max_chars);
    if ctx.max_chars <= 1_200 {
        // Under very small budgets, prefer a smaller snippet payload so we can keep at least one
        // snippet alongside `project_facts` without popping sections during trimming.
        doc_max_chars = doc_max_chars.clamp(160, 320);
    } else if response_mode != ResponseMode::Full {
        // In low-noise modes, snippets are returned inline in the `read_pack` JSON payload.
        // JSON escaping and per-section key overhead can exceed the envelope headroom estimate
        // under small budgets, causing the final trimming pass to drop an entire snippet.
        //
        // Agent-native behavior: prefer slightly smaller snippets so the pack more often fits
        // 2+ "must-have" sections (e.g. AGENTS + README) instead of losing one to trimming.
        let (num, den) = if ctx.max_chars <= 2_500 {
            (2usize, 3usize) // tighter budgets need more headroom
        } else if ctx.max_chars <= 5_000 {
            (3usize, 4usize)
        } else {
            (4usize, 5usize)
        };
        doc_max_chars = (doc_max_chars.saturating_mul(num) / den)
            .clamp(160, 6_000)
            .min(ctx.inner_max_chars);
    }

    let candidates = collect_memory_file_candidates(&ctx.root);
    if start_candidate_index > candidates.len() {
        return Err(call_error("invalid_cursor", "Invalid cursor: out of range"));
    }

    if let Some(rel) = focus_file.as_deref() {
        let focus_max_lines = 140usize;
        let start_line =
            memory_best_start_line(&ctx.root, rel, focus_max_lines, snippet_kind_for_path(rel));
        if let Ok(slice) = compute_file_slice_result(
            &ctx.root,
            &ctx.root_display,
            &FileSliceRequest {
                path: None,
                file: Some(rel.to_string()),
                start_line: Some(start_line),
                max_lines: Some(focus_max_lines),
                max_chars: Some(focus_reserved_chars),
                format: None,
                response_mode: Some(response_mode),
                allow_secrets: request.allow_secrets,
                cursor: None,
            },
        ) {
            // Insert after project_facts and any external_memory overlays (if present).
            let mut insert_idx = sections
                .iter()
                .position(|section| matches!(section, ReadPackSection::ProjectFacts { .. }))
                .map(|idx| idx + 1)
                .unwrap_or(0);
            while insert_idx < sections.len()
                && matches!(sections[insert_idx], ReadPackSection::ExternalMemory { .. })
            {
                insert_idx += 1;
            }

            let kind = if response_mode == ResponseMode::Minimal {
                None
            } else {
                Some(snippet_kind_for_path(rel))
            };
            sections.insert(
                insert_idx,
                ReadPackSection::Snippet {
                    result: ReadPackSnippet {
                        file: slice.file.clone(),
                        start_line: slice.start_line,
                        end_line: slice.end_line,
                        content: slice.content.clone(),
                        kind,
                        reason: Some(REASON_ANCHOR_FOCUS_FILE.to_string()),
                        next_cursor: None,
                    },
                },
            );
        }
    }

    let mut next_candidate_index: Option<usize> = None;
    if docs_limit > 0 {
        let mut added_docs = 0usize;
        let allow_working_set_bias = trimmed_non_empty_str(request.cursor.as_deref()).is_none();
        let seen: HashSet<String> = if allow_working_set_bias {
            let session = service.session.lock().await;
            session.seen_snippet_files_set_snapshot()
        } else {
            HashSet::new()
        };
        let mut deferred_seen: Vec<(usize, String)> = Vec::new();

        for (idx, rel) in candidates.iter().enumerate().skip(start_candidate_index) {
            if added_docs >= docs_limit {
                next_candidate_index = Some(idx);
                break;
            }

            let is_anchor_doc = allow_working_set_bias && idx < 2;
            if allow_working_set_bias && !is_anchor_doc && seen.contains(rel) {
                deferred_seen.push((idx, rel.clone()));
                continue;
            }

            let start_line =
                memory_best_start_line(&ctx.root, rel, doc_max_lines, snippet_kind_for_path(rel));
            let Ok(mut slice) = compute_file_slice_result(
                &ctx.root,
                &ctx.root_display,
                &FileSliceRequest {
                    path: None,
                    file: Some(rel.clone()),
                    start_line: Some(start_line),
                    max_lines: Some(doc_max_lines),
                    max_chars: Some(doc_max_chars),
                    format: None,
                    response_mode: Some(response_mode),
                    allow_secrets: request.allow_secrets,
                    cursor: None,
                },
            ) else {
                continue;
            };

            if response_mode == ResponseMode::Full {
                if let Some(cursor) = slice.next_cursor.take() {
                    slice.next_cursor = Some(compact_cursor_alias(service, cursor).await);
                }
            }

            if response_mode == ResponseMode::Full {
                sections.push(ReadPackSection::FileSlice { result: slice });
            } else {
                let kind = if response_mode == ResponseMode::Minimal {
                    None
                } else {
                    Some(snippet_kind_for_path(rel))
                };
                sections.push(ReadPackSection::Snippet {
                    result: ReadPackSnippet {
                        file: slice.file.clone(),
                        start_line: slice.start_line,
                        end_line: slice.end_line,
                        content: slice.content.clone(),
                        kind,
                        reason: Some(REASON_ANCHOR_DOC.to_string()),
                        next_cursor: None,
                    },
                });
            }
            added_docs += 1;
        }

        // If we skipped too many already-seen docs and ran out of unseen options, backfill from
        // the deferred list (preserving candidate order).
        if added_docs < docs_limit {
            for (_, rel) in deferred_seen {
                if added_docs >= docs_limit {
                    break;
                }

                let start_line = memory_best_start_line(
                    &ctx.root,
                    &rel,
                    doc_max_lines,
                    snippet_kind_for_path(&rel),
                );
                let Ok(mut slice) = compute_file_slice_result(
                    &ctx.root,
                    &ctx.root_display,
                    &FileSliceRequest {
                        path: None,
                        file: Some(rel.clone()),
                        start_line: Some(start_line),
                        max_lines: Some(doc_max_lines),
                        max_chars: Some(doc_max_chars),
                        format: None,
                        response_mode: Some(response_mode),
                        allow_secrets: request.allow_secrets,
                        cursor: None,
                    },
                ) else {
                    continue;
                };

                if response_mode == ResponseMode::Full {
                    if let Some(cursor) = slice.next_cursor.take() {
                        slice.next_cursor = Some(compact_cursor_alias(service, cursor).await);
                    }
                }

                if response_mode == ResponseMode::Full {
                    sections.push(ReadPackSection::FileSlice { result: slice });
                } else {
                    let kind = if response_mode == ResponseMode::Minimal {
                        None
                    } else {
                        Some(snippet_kind_for_path(&rel))
                    };
                    sections.push(ReadPackSection::Snippet {
                        result: ReadPackSnippet {
                            file: slice.file.clone(),
                            start_line: slice.start_line,
                            end_line: slice.end_line,
                            content: slice.content.clone(),
                            kind,
                            reason: Some(REASON_ANCHOR_DOC.to_string()),
                            next_cursor: None,
                        },
                    });
                }
                added_docs += 1;
            }
        }
    }

    let mut entrypoint_section: Option<ReadPackSection> = None;
    if !entrypoint_done && wants_entrypoint {
        if let Some(rel) = entrypoint_file {
            let entry_max_lines = 160usize;
            let entry_max_chars = (ctx.inner_max_chars / 8)
                .clamp(240, 3_000)
                .min(ctx.inner_max_chars);
            let start_line = memory_best_start_line(
                &ctx.root,
                &rel,
                entry_max_lines,
                snippet_kind_for_path(&rel),
            );

            if let Ok(mut slice) = compute_file_slice_result(
                &ctx.root,
                &ctx.root_display,
                &FileSliceRequest {
                    path: None,
                    file: Some(rel.clone()),
                    start_line: Some(start_line),
                    max_lines: Some(entry_max_lines),
                    max_chars: Some(entry_max_chars),
                    format: None,
                    response_mode: Some(response_mode),
                    allow_secrets: request.allow_secrets,
                    cursor: None,
                },
            ) {
                if response_mode == ResponseMode::Full {
                    if let Some(cursor) = slice.next_cursor.take() {
                        slice.next_cursor = Some(compact_cursor_alias(service, cursor).await);
                    }
                }

                entrypoint_done = true;
                entrypoint_section = Some(if response_mode == ResponseMode::Full {
                    ReadPackSection::FileSlice { result: slice }
                } else {
                    let kind = if response_mode == ResponseMode::Minimal {
                        None
                    } else {
                        Some(snippet_kind_for_path(&rel))
                    };
                    ReadPackSection::Snippet {
                        result: ReadPackSnippet {
                            file: slice.file.clone(),
                            start_line: slice.start_line,
                            end_line: slice.end_line,
                            content: slice.content.clone(),
                            kind,
                            reason: Some(REASON_ANCHOR_ENTRYPOINT.to_string()),
                            next_cursor: None,
                        },
                    }
                });
            }
        }
    }

    if let Some(section) = entrypoint_section {
        let pre_entry_snippets = if ctx.max_chars <= 6_000 { 1 } else { 2 };
        let mut seen_payload = 0usize;
        let mut insert_idx = sections.len();
        for (idx, section) in sections.iter().enumerate().skip(1) {
            match section {
                ReadPackSection::Snippet { .. } | ReadPackSection::FileSlice { .. } => {
                    seen_payload += 1;
                    if seen_payload >= pre_entry_snippets {
                        insert_idx = (idx + 1).min(sections.len());
                        break;
                    }
                }
                _ => {}
            }
        }
        sections.insert(insert_idx, section);
    }

    if let Some(next_index) = next_candidate_index {
        if next_index < candidates.len() {
            let cursor = ReadPackMemoryCursorV1 {
                v: CURSOR_VERSION,
                tool: "read_pack".to_string(),
                mode: "memory".to_string(),
                root: Some(ctx.root_display.clone()),
                root_hash: Some(cursor_fingerprint(&ctx.root_display)),
                max_chars: Some(ctx.max_chars),
                response_mode: Some(response_mode),
                next_candidate_index: next_index,
                entrypoint_done,
            };
            if let Ok(token) = encode_cursor(&cursor) {
                let compact = compact_cursor_alias(service, token).await;
                *next_cursor_out = Some(compact);
            } else {
                *next_cursor_out = None;
            }

            if response_mode == ResponseMode::Full {
                if let Some(next_cursor) = next_cursor_out.as_deref() {
                    next_actions.push(ReadPackNextAction {
                        tool: "read_pack".to_string(),
                        args: json!({
                            "path": ctx.root_display.clone(),
                            "max_chars": ctx.max_chars,
                            "cursor": next_cursor,
                        }),
                        reason: "Continue the memory-pack (next page of high-signal snippets)."
                            .to_string(),
                    });
                }
            }
        }
    }

    Ok(())
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct ReadPackMemoryCursorV1 {
    v: u32,
    tool: String,
    mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_hash: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_mode: Option<ResponseMode>,
    next_candidate_index: usize,
    entrypoint_done: bool,
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct ReadPackRecallCursorV1 {
    v: u32,
    tool: String,
    mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_hash: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_mode: Option<ResponseMode>,
    questions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    topics: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    include_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    exclude_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prefer_code: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include_docs: Option<bool>,
    #[serde(default)]
    allow_secrets: bool,
    next_question_index: usize,
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct ReadPackRecallCursorStoredV1 {
    v: u32,
    tool: String,
    mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_hash: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_mode: Option<ResponseMode>,
    store_id: u64,
}

const MAX_RECALL_QUESTIONS: usize = 12;
const MAX_RECALL_QUESTION_CHARS: usize = 220;
const MAX_RECALL_QUESTION_BYTES: usize = 384;
const MAX_RECALL_TOPICS: usize = 8;
const MAX_RECALL_TOPIC_CHARS: usize = 80;
const MAX_RECALL_TOPIC_BYTES: usize = 192;
const DEFAULT_RECALL_SNIPPETS_PER_QUESTION: usize = 3;
const MAX_RECALL_SNIPPETS_PER_QUESTION: usize = 5;

fn trim_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max_chars {
            break;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

fn trim_utf8_bytes(s: &str, max_bytes: usize) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= max_bytes {
        return trimmed.to_string();
    }

    let mut end = max_bytes.min(trimmed.len());
    while end > 0 && !trimmed.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    trimmed[..end].trim().to_string()
}

fn normalize_questions(request: &ReadPackRequest) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(questions) = request.questions.as_ref() {
        for q in questions {
            let q = q.trim();
            if q.is_empty() {
                continue;
            }
            let q = trim_chars(q, MAX_RECALL_QUESTION_CHARS);
            out.push(trim_utf8_bytes(&q, MAX_RECALL_QUESTION_BYTES));
            if out.len() >= MAX_RECALL_QUESTIONS {
                break;
            }
        }
    }

    if out.is_empty() {
        if let Some(ask) = trimmed_non_empty_str(request.ask.as_deref()) {
            let lines: Vec<&str> = ask.lines().collect();
            if lines.len() > 1 {
                for line in lines {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let q = trim_chars(line, MAX_RECALL_QUESTION_CHARS);
                    out.push(trim_utf8_bytes(&q, MAX_RECALL_QUESTION_BYTES));
                    if out.len() >= MAX_RECALL_QUESTIONS {
                        break;
                    }
                }
            } else {
                let q = trim_chars(ask, MAX_RECALL_QUESTION_CHARS);
                out.push(trim_utf8_bytes(&q, MAX_RECALL_QUESTION_BYTES));
            }
        }
    }

    out
}

fn normalize_topics(request: &ReadPackRequest) -> Option<Vec<String>> {
    let topics = request.topics.as_ref()?;

    let mut out = Vec::new();
    for topic in topics {
        let topic = topic.trim();
        if topic.is_empty() {
            continue;
        }
        let topic = trim_chars(topic, MAX_RECALL_TOPIC_CHARS);
        out.push(trim_utf8_bytes(&topic, MAX_RECALL_TOPIC_BYTES));
        if out.len() >= MAX_RECALL_TOPICS {
            break;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

const MAX_RECALL_FILTER_PATHS: usize = 16;
const MAX_RECALL_FILTER_PATH_BYTES: usize = 120;

fn normalize_path_prefix_list(raw: Option<&Vec<String>>) -> Vec<String> {
    let Some(values) = raw else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        out.push(trim_utf8_bytes(value, MAX_RECALL_FILTER_PATH_BYTES));
        if out.len() >= MAX_RECALL_FILTER_PATHS {
            break;
        }
    }
    out
}

fn normalize_optional_pattern(raw: Option<&str>) -> Option<String> {
    trimmed_non_empty_str(raw).map(|value| trim_utf8_bytes(value, MAX_RECALL_FILTER_PATH_BYTES))
}

fn snippet_kind_for_path(path: &str) -> ReadPackSnippetKind {
    let normalized = path.replace('\\', "/");
    let file_name = Path::new(&normalized)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();

    if file_name.ends_with(".md")
        || file_name.ends_with(".mdx")
        || file_name.ends_with(".rst")
        || file_name.ends_with(".adoc")
        || file_name.ends_with(".txt")
        || file_name.ends_with(".context")
    {
        return ReadPackSnippetKind::Doc;
    }

    if file_name.starts_with('.') {
        return ReadPackSnippetKind::Config;
    }

    let ext = Path::new(&file_name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_lowercase();
    if matches!(
        ext.as_str(),
        "toml" | "json" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "properties" | "env"
    ) {
        return ReadPackSnippetKind::Config;
    }

    if file_name == "dockerfile"
        || file_name == "docker-compose.yml"
        || file_name == "docker-compose.yaml"
        || file_name == "makefile"
        || file_name == "justfile"
    {
        return ReadPackSnippetKind::Config;
    }

    ReadPackSnippetKind::Code
}

fn parse_path_token(token: &str) -> Option<(String, Option<usize>)> {
    let token = token.trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}')
    });
    let token = token.trim_matches(|c: char| matches!(c, ',' | ';' | ':' | '.' | '?'));
    if token.is_empty() {
        return None;
    }

    let token = token.replace('\\', "/");
    let token = token.strip_prefix("./").unwrap_or(&token);
    if token.starts_with('/') || token.contains("..") {
        return None;
    }

    // Parse `path:line` if line is numeric.
    if let Some((left, right)) = token.rsplit_once(':') {
        if let Ok(line) = right.parse::<usize>() {
            let left = left.trim();
            if !left.is_empty() && !left.contains(':') {
                return Some((left.to_string(), Some(line)));
            }
        }
    }

    Some((token.to_string(), None))
}

fn extract_existing_file_ref(
    question: &str,
    root: &Path,
    allow_secrets: bool,
) -> Option<(String, Option<usize>)> {
    let mut best: Option<(String, Option<usize>)> = None;
    for raw in question.split_whitespace() {
        let Some((candidate, line)) = parse_path_token(raw) else {
            continue;
        };
        if !allow_secrets && is_disallowed_memory_file(&candidate) {
            continue;
        }
        let full = root.join(&candidate);
        if full.is_file() {
            best = Some((candidate, line));
            break;
        }
    }
    best
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpsIntent {
    TestAndGates,
    Snapshots,
    Run,
    Build,
    Deploy,
    Setup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecallStructuralIntent {
    ProjectIdentity,
    EntryPoints,
    Contracts,
    Configuration,
}

fn recall_structural_intent(question: &str) -> Option<RecallStructuralIntent> {
    let q = question.to_lowercase();

    let is_identity = [
        "what is this project",
        "what is this repo",
        "what is this",
        "about this project",
        "описание проекта",
        "что это за проект",
        "что это",
        "о проекте",
    ]
    .iter()
    .any(|needle| q.contains(needle));

    if is_identity {
        return Some(RecallStructuralIntent::ProjectIdentity);
    }

    let is_entrypoints = [
        "entry point",
        "entrypoint",
        "entry points",
        "точка входа",
        "точки входа",
        "main entry",
        "main app entry",
        "binaries",
        "binary",
        "bins",
        "bin ",
        "where is main",
        "где main",
    ]
    .iter()
    .any(|needle| q.contains(needle));

    if is_entrypoints {
        return Some(RecallStructuralIntent::EntryPoints);
    }

    let is_contracts = [
        "contract",
        "contracts",
        "protocol",
        "openapi",
        "grpc",
        "proto",
        "schema",
        "spec",
        "контракт",
        "контракты",
        "протокол",
        "спека",
        "схема",
    ]
    .iter()
    .any(|needle| q.contains(needle));

    if is_contracts {
        return Some(RecallStructuralIntent::Contracts);
    }

    let is_config = [
        "configuration",
        "config",
        "settings",
        "where is config",
        "how is config",
        ".env",
        "yaml",
        "toml",
        "конфиг",
        "настройк",
        "где конфиг",
    ]
    .iter()
    .any(|needle| q.contains(needle));

    if is_config {
        return Some(RecallStructuralIntent::Configuration);
    }

    None
}

fn recall_doc_candidate_score(rel: &str) -> i32 {
    let normalized = rel.replace('\\', "/").to_ascii_lowercase();
    match normalized.as_str() {
        "readme.md" => 300,
        "agents.md" => 290,
        "docs/quick_start.md" => 280,
        "docs/readme.md" => 275,
        "development.md" => 270,
        "contributing.md" => 260,
        "architecture.md" => 255,
        "docs/architecture.md" => 250,
        "philosophy.md" => 240,
        _ if normalized.ends_with("/readme.md") => 220,
        _ if normalized.ends_with("/agents.md") => 210,
        _ if normalized.ends_with("/docs/quick_start.md") => 205,
        _ if normalized.ends_with(".md") => 120,
        _ => 10,
    }
}

fn entrypoint_candidate_score(rel: &str) -> i32 {
    let normalized = rel.replace('\\', "/").to_ascii_lowercase();
    match normalized.as_str() {
        "src/main.rs" => 300,
        "src/lib.rs" => 260,
        "main.go" | "main.py" | "app.py" => 250,
        "src/main.py" | "src/app.py" => 240,
        "src/index.ts" | "src/index.js" => 230,
        "src/main.ts" | "src/main.js" => 225,
        "cargo.toml" | "package.json" | "pyproject.toml" | "go.mod" => 200,
        _ if normalized.ends_with("/src/main.rs") => 190,
        _ if normalized.ends_with("/src/lib.rs") => 170,
        _ if normalized.ends_with("/src/index.ts")
            || normalized.ends_with("/src/index.js")
            || normalized.ends_with("/src/main.ts")
            || normalized.ends_with("/src/main.js") =>
        {
            165
        }
        _ if normalized.ends_with("/main.go")
            || normalized.ends_with("/main.py")
            || normalized.ends_with("/app.py") =>
        {
            160
        }
        _ if normalized.contains("xtask") && normalized.ends_with("/src/main.rs") => 210,
        _ => 10,
    }
}

fn contract_candidate_score(rel: &str) -> i32 {
    let normalized = rel.replace('\\', "/").to_ascii_lowercase();
    match normalized.as_str() {
        "docs/contracts/protocol.md" => 300,
        "docs/contracts/readme.md" => 280,
        "contracts/http/v1/openapi.json" => 260,
        "contracts/http/v1/openapi.yaml" | "contracts/http/v1/openapi.yml" => 255,
        "openapi.json" | "openapi.yaml" | "openapi.yml" => 250,
        "proto/command.proto" => 240,
        "architecture.md" | "docs/architecture.md" => 220,
        "readme.md" => 210,
        _ if normalized.starts_with("docs/contracts/") && normalized.ends_with(".md") => 200,
        _ if normalized.starts_with("contracts/") => 180,
        _ if normalized.starts_with("proto/") && normalized.ends_with(".proto") => 170,
        _ => 10,
    }
}

fn recall_structural_candidates(
    intent: RecallStructuralIntent,
    root: &Path,
    facts: &ProjectFactsResult,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = HashSet::new();

    let mut push = |rel: &str| {
        let rel = rel.trim().replace('\\', "/");
        if rel.is_empty() || rel == "." {
            return;
        }
        if is_disallowed_memory_file(&rel) {
            return;
        }
        if !root.join(&rel).is_file() {
            return;
        }
        if seen.insert(rel.clone()) {
            out.push(rel);
        }
    };

    match intent {
        RecallStructuralIntent::ProjectIdentity => {
            for rel in [
                "README.md",
                "docs/README.md",
                "AGENTS.md",
                "PHILOSOPHY.md",
                "ARCHITECTURE.md",
                "docs/ARCHITECTURE.md",
                "docs/QUICK_START.md",
                "DEVELOPMENT.md",
                "CONTRIBUTING.md",
            ] {
                push(rel);
            }

            // If the root is a wrapper, surface module docs as well (bounded, deterministic).
            for module in facts.modules.iter().take(6) {
                for rel in ["README.md", "AGENTS.md", "docs/README.md"] {
                    push(&format!("{module}/{rel}"));
                }
            }

            out.sort_by(|a, b| {
                recall_doc_candidate_score(b)
                    .cmp(&recall_doc_candidate_score(a))
                    .then_with(|| a.cmp(b))
            });
        }
        RecallStructuralIntent::EntryPoints => {
            // Start with manifest-level hints, then actual code entrypoints.
            for rel in [
                "Cargo.toml",
                "package.json",
                "pyproject.toml",
                "go.mod",
                "README.md",
            ] {
                push(rel);
            }

            for rel in &facts.entry_points {
                push(rel);
            }

            // If project_facts didn't find module entrypoints, derive a few from module roots.
            for module in facts.modules.iter().take(12) {
                for rel in [
                    "src/main.rs",
                    "src/lib.rs",
                    "main.go",
                    "main.py",
                    "app.py",
                    "src/main.py",
                    "src/app.py",
                    "src/index.ts",
                    "src/index.js",
                    "src/main.ts",
                    "src/main.js",
                ] {
                    push(&format!("{module}/{rel}"));
                }
            }

            out.sort_by(|a, b| {
                entrypoint_candidate_score(b)
                    .cmp(&entrypoint_candidate_score(a))
                    .then_with(|| a.cmp(b))
            });
        }
        RecallStructuralIntent::Contracts => {
            for rel in [
                "docs/contracts/protocol.md",
                "docs/contracts/README.md",
                "docs/contracts/runtime.md",
                "docs/contracts/quality_gates.md",
                "ARCHITECTURE.md",
                "docs/ARCHITECTURE.md",
                "README.md",
                "proto/command.proto",
                "contracts/http/v1/openapi.json",
                "contracts/http/v1/openapi.yaml",
                "contracts/http/v1/openapi.yml",
                "openapi.json",
                "openapi.yaml",
                "openapi.yml",
            ] {
                push(rel);
            }

            // If there are contract dirs, surface one or two stable "front door" docs from them.
            for module in facts
                .contracts
                .iter()
                .filter(|c| c.ends_with('/') || root.join(c).is_dir())
                .take(4)
            {
                for rel in ["README.md", "readme.md"] {
                    push(&format!("{module}/{rel}"));
                }
            }

            out.sort_by(|a, b| {
                contract_candidate_score(b)
                    .cmp(&contract_candidate_score(a))
                    .then_with(|| a.cmp(b))
            });
        }
        RecallStructuralIntent::Configuration => {
            // Doc hints first (what config is used), then the concrete config files.
            for rel in ["README.md", "docs/QUICK_START.md", "DEVELOPMENT.md"] {
                push(rel);
            }

            for rel in &facts.key_configs {
                push(rel);
            }

            for rel in [
                "config/.env.example",
                "config/.env.sample",
                "config/.env.template",
                "config/.env.dist",
                "config/docker-compose.yml",
                "config/docker-compose.yaml",
                "configs/.env.example",
                "configs/docker-compose.yml",
                "configs/docker-compose.yaml",
                "config/config.yml",
                "config/config.yaml",
                "config/settings.yml",
                "config/settings.yaml",
                "configs/config.yml",
                "configs/config.yaml",
                "configs/settings.yml",
                "configs/settings.yaml",
            ] {
                push(rel);
            }

            out.sort_by(|a, b| {
                config_candidate_score(b)
                    .cmp(&config_candidate_score(a))
                    .then_with(|| a.cmp(b))
            });
        }
    }

    out
}

fn ops_intent(question: &str) -> Option<OpsIntent> {
    let q = question.to_lowercase();

    let contains_ascii_token = |needle: &str| {
        q.split(|c: char| !c.is_ascii_alphanumeric())
            .any(|tok| tok == needle)
    };

    // Highly specific ops: visual regression / golden snapshot workflows.
    //
    // Keep it strict: require snapshot/golden keywords (GPU alone should not redirect from "run").
    let mentions_snapshots = [
        "snapshot",
        "snapshots",
        "golden",
        "goldens",
        "baseline",
        "screenshot",
        "visual regression",
        "update_snapshots",
        "update-snapshots",
        "update_snapshot",
        "update-snapshot",
        "снапшот",
        "скриншот",
        "голден",
    ]
    .iter()
    .any(|needle| q.contains(needle));
    if mentions_snapshots {
        return Some(OpsIntent::Snapshots);
    }

    let mentions_quality = [
        "quality gate",
        "quality gates",
        "quality-gate",
        "quality_gates",
        "quality",
        "гейт",
        "гейты",
        "проверки",
        "линт",
        "lint",
        "clippy",
        "fmt",
        "format",
        "validate_contracts",
    ]
    .iter()
    .any(|needle| q.contains(needle));

    let mentions_test = [
        "test",
        "tests",
        "testing",
        "pytest",
        "cargo test",
        "go test",
        "npm test",
        "yarn test",
        "pnpm test",
        "тест",
    ]
    .iter()
    .any(|needle| q.contains(needle));

    // Avoid substring false-positives ("velocity" contains "ci"). Prefer token detection for CI.
    if mentions_quality || mentions_test || contains_ascii_token("ci") || q.contains("pipeline") {
        return Some(OpsIntent::TestAndGates);
    }

    if [
        "run",
        "start",
        "serve",
        "dev",
        "local",
        "launch",
        "запуск",
        "запустить",
        "старт",
        "локально",
    ]
    .iter()
    .any(|needle| q.contains(needle))
    {
        return Some(OpsIntent::Run);
    }

    if ["build", "compile", "собрат", "сборк"]
        .iter()
        .any(|needle| q.contains(needle))
    {
        return Some(OpsIntent::Build);
    }

    if [
        "deploy",
        "release",
        "prod",
        "production",
        "депло",
        "разверн",
        "релиз",
    ]
    .iter()
    .any(|needle| q.contains(needle))
    {
        return Some(OpsIntent::Deploy);
    }

    if [
        "install",
        "setup",
        "configure",
        "init",
        "установ",
        "настро",
        "конфиг",
    ]
    .iter()
    .any(|needle| q.contains(needle))
    {
        return Some(OpsIntent::Setup);
    }

    None
}

fn ops_grep_pattern(intent: OpsIntent) -> &'static str {
    match intent {
        OpsIntent::TestAndGates => {
            // Prefer concrete commands / recipes across ecosystems.
            r"(?m)(^\s*(test|tests|check|gate|lint|fmt|format)\s*:|scripts/validate_contracts\.sh|validate_contracts|cargo\s+fmt\b|fmt\b.*--check|cargo\s+clippy\b|clippy\b.*--workspace|cargo\s+xtask\s+(check|gate)\b|cargo\s+test\b|CONTEXT_FINDER_EMBEDDING_MODE=stub\s+cargo\s+test\b|cargo\s+nextest\b|pytest\b|go\s+test\b|npm\s+test\b|yarn\s+test\b|pnpm\s+test\b|just\s+(test|check|gate|lint|fmt)\b|make\s+test\b|make\s+check\b)"
        }
        OpsIntent::Snapshots => {
            // Visual regression / golden snapshot workflows across ecosystems.
            // Prefer actionable "update baseline" commands and env knobs.
            r"(?mi)(snapshot|snapshots|golden|goldens|baseline|screenshot|visual\s+regression|update[_-]?snapshots|--update[-_]?snapshots|update[_-]?snapshot|--update[-_]?snapshot|update[_-]?baseline|--update[-_]?baseline|record[_-]?snapshots|APEX_UPDATE_SNAPSHOTS|UPDATE_SNAPSHOTS|SNAPSHOT|GOLDEN|baseline\s+image)"
        }
        OpsIntent::Run => {
            r"(?m)(^\s*(run|start|dev|serve)\s*:|cargo\s+run\b|python\s+-m\b|uv\s+run\b|poetry\s+run\b|npm\s+run\s+dev\b|npm\s+start\b|yarn\s+dev\b|pnpm\s+dev\b|just\s+(run|start|dev)\b|make\s+run\b|docker\s+compose\s+up\b)"
        }
        OpsIntent::Build => {
            r"(?m)(^\s*(build|compile)\s*:|cargo\s+build\b|go\s+build\b|python\s+-m\s+build\b|npm\s+run\s+build\b|yarn\s+build\b|pnpm\s+build\b|just\s+build\b|make\s+build\b|cmake\b|bazel\b)"
        }
        OpsIntent::Deploy => {
            r"(?m)(^\s*(deploy|release|prod)\s*:|deploy\b|release\b|docker\s+build\b|docker\s+compose\b|kubectl\b|helm\b|terraform\b)"
        }
        OpsIntent::Setup => {
            r"(?m)(^\s*(install|setup|init|configure)\s*:|pip\s+install\b|poetry\s+install\b|uv\s+sync\b|npm\s+install\b|pnpm\s+install\b|yarn\b\s+install\b|cargo\s+install\b|just\s+install\b|make\s+install\b)"
        }
    }
}

fn best_keyword_pattern(question: &str) -> Option<String> {
    let mut best: Option<String> = None;
    for token in question
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .filter(|t| t.len() >= 3)
    {
        if token.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let lowered = token.to_lowercase();
        if matches!(
            lowered.as_str(),
            "the"
                | "and"
                | "with"
                | "for"
                | "from"
                | "that"
                | "this"
                | "как"
                | "что"
                | "где"
                | "чем"
                | "когда"
                | "почему"
                | "который"
                | "которая"
                | "которые"
        ) {
            continue;
        }
        let replace = match best.as_ref() {
            None => true,
            Some(current) => token.len() > current.len(),
        };
        if replace {
            best = Some(token.to_string());
        }
    }
    best.map(|kw| regex::escape(&kw))
}

fn recall_question_tokens(question: &str) -> Vec<String> {
    // Deterministic, Unicode-friendly tokenization for lightweight relevance scoring.
    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();

    let flush = |out: &mut Vec<String>, buf: &mut String| {
        if buf.is_empty() {
            return;
        }
        let token = buf.to_lowercase();
        buf.clear();

        if token.len() < 3 {
            return;
        }
        if token.chars().all(|c| c.is_ascii_digit()) {
            return;
        }
        if matches!(
            token.as_str(),
            "the"
                | "and"
                | "with"
                | "for"
                | "from"
                | "that"
                | "this"
                | "как"
                | "что"
                | "где"
                | "чем"
                | "когда"
                | "почему"
                | "который"
                | "которая"
                | "которые"
                | "зачем"
                | "есть"
                | "про"
                | "или"
                | "над"
        ) {
            return;
        }
        if out.iter().any(|existing| existing == &token) {
            return;
        }
        if out.len() >= 12 {
            return;
        }
        out.push(token);
    };

    for ch in question.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            buf.push(ch);
            continue;
        }
        flush(&mut out, &mut buf);
        if out.len() >= 12 {
            break;
        }
    }
    flush(&mut out, &mut buf);

    out
}

fn score_recall_snippet(question_tokens: &[String], snippet: &ReadPackSnippet) -> i32 {
    if question_tokens.is_empty() {
        return 0;
    }
    let file = snippet.file.to_ascii_lowercase();
    let content = snippet.content.to_lowercase();
    let mut score = 0i32;

    for token in question_tokens {
        if file.contains(token) {
            score += 3;
        }
        if content.contains(token) {
            score += 5;
        }
    }

    // Small heuristic boost: snippets with runnable commands are usually better for ops recall.
    if content.contains("cargo ") || content.contains("npm ") || content.contains("yarn ") {
        score += 1;
    }
    if content.contains("docker ") || content.contains("kubectl ") || content.contains("make ") {
        score += 1;
    }

    score
}

fn recall_has_code_snippet(snippets: &[ReadPackSnippet]) -> bool {
    snippets
        .iter()
        .any(|snippet| snippet_kind_for_path(&snippet.file) == ReadPackSnippetKind::Code)
}

fn recall_code_scope_candidates(root: &Path, facts: &ProjectFactsResult) -> Vec<String> {
    // A small, deterministic set of "likely code lives here" roots used as a second-pass scope
    // for precision grep (avoids README/docs-first matches when snippet_limit is tight).
    let mut out: Vec<String> = Vec::new();

    // Prefer project-specific knowledge when available (facts.key_dirs is already bounded).
    for dir in &facts.key_dirs {
        let dir = dir.trim().replace('\\', "/");
        if dir.is_empty() || dir.starts_with('.') {
            continue;
        }
        if matches!(
            dir.as_str(),
            "src"
                | "crates"
                | "packages"
                | "apps"
                | "services"
                | "lib"
                | "libs"
                | "backend"
                | "frontend"
                | "server"
                | "client"
        ) && root.join(&dir).is_dir()
        {
            out.push(dir);
        }
        if out.len() >= 6 {
            break;
        }
    }

    // Fallback: common container directories (covers thin wrappers where key_dirs is noisy).
    if out.is_empty() {
        for dir in [
            "src", "crates", "packages", "apps", "services", "lib", "libs",
        ] {
            if root.join(dir).is_dir() {
                out.push(dir.to_string());
            }
            if out.len() >= 6 {
                break;
            }
        }
    }

    out
}

fn recall_keyword_patterns(question_tokens: &[String]) -> Vec<String> {
    let mut tokens: Vec<String> = question_tokens.to_vec();
    tokens.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    tokens.dedup();

    let mut out = Vec::new();
    for token in tokens {
        if token.len() < 3 {
            continue;
        }
        if out.iter().any(|p: &String| p == &token) {
            continue;
        }
        out.push(regex::escape(&token));
        if out.len() >= 2 {
            break;
        }
    }
    out
}

struct RecallCodeUpgradeParams<'a> {
    ctx: &'a ReadPackContext,
    facts_snapshot: &'a ProjectFactsResult,
    question_tokens: &'a [String],
    snippet_limit: usize,
    snippet_max_chars: usize,
    grep_context_lines: usize,
    include_paths: &'a [String],
    exclude_paths: &'a [String],
    file_pattern: Option<&'a str>,
    allow_secrets: bool,
}

async fn recall_upgrade_to_code_snippets(
    params: RecallCodeUpgradeParams<'_>,
    snippets: &mut Vec<ReadPackSnippet>,
) -> ToolResult<()> {
    if snippets.is_empty() || recall_has_code_snippet(snippets) {
        return Ok(());
    }

    let patterns = recall_keyword_patterns(params.question_tokens);
    if patterns.is_empty() {
        return Ok(());
    }

    let probe_hunks = params
        .snippet_limit
        .saturating_mul(8)
        .clamp(2, MAX_RECALL_SNIPPETS_PER_QUESTION);

    let mut found_code: Vec<ReadPackSnippet> = Vec::new();
    for (idx, pattern) in patterns.iter().enumerate() {
        let (mut found, _cursor) = snippets_from_grep_filtered(
            params.ctx,
            pattern,
            GrepSnippetParams {
                file: None,
                file_pattern: params.file_pattern.map(|p| p.to_string()),
                before: params.grep_context_lines,
                after: params.grep_context_lines,
                max_hunks: probe_hunks,
                max_chars: params.snippet_max_chars,
                case_sensitive: false,
                allow_secrets: params.allow_secrets,
            },
            params.include_paths,
            params.exclude_paths,
            params.file_pattern,
        )
        .await?;

        found.retain(|snippet| snippet_kind_for_path(&snippet.file) == ReadPackSnippetKind::Code);
        if !found.is_empty() {
            found_code = found;
            break;
        }

        // Second attempt: narrow grep to likely code roots if the caller didn't explicitly scope.
        if idx == 0 && params.include_paths.is_empty() {
            let code_scopes = recall_code_scope_candidates(&params.ctx.root, params.facts_snapshot);
            if !code_scopes.is_empty() {
                let (mut scoped, _cursor) = snippets_from_grep_filtered(
                    params.ctx,
                    pattern,
                    GrepSnippetParams {
                        file: None,
                        file_pattern: params.file_pattern.map(|p| p.to_string()),
                        before: params.grep_context_lines,
                        after: params.grep_context_lines,
                        max_hunks: probe_hunks,
                        max_chars: params.snippet_max_chars,
                        case_sensitive: false,
                        allow_secrets: params.allow_secrets,
                    },
                    &code_scopes,
                    params.exclude_paths,
                    params.file_pattern,
                )
                .await?;
                scoped.retain(|snippet| {
                    snippet_kind_for_path(&snippet.file) == ReadPackSnippetKind::Code
                });
                if !scoped.is_empty() {
                    found_code = scoped;
                    break;
                }
            }
        }
    }

    if found_code.is_empty() {
        return Ok(());
    }

    let mut seen: HashSet<(String, usize, usize)> = HashSet::new();
    let mut merged: Vec<ReadPackSnippet> = Vec::new();
    for snippet in std::mem::take(snippets)
        .into_iter()
        .chain(found_code.into_iter())
    {
        let key = (snippet.file.clone(), snippet.start_line, snippet.end_line);
        if seen.insert(key) {
            merged.push(snippet);
        }
    }

    merged.sort_by(|a, b| {
        let a_kind = snippet_kind_for_path(&a.file);
        let b_kind = snippet_kind_for_path(&b.file);
        let a_rank = match a_kind {
            ReadPackSnippetKind::Code => 0,
            ReadPackSnippetKind::Config => 1,
            ReadPackSnippetKind::Doc => 2,
        };
        let b_rank = match b_kind {
            ReadPackSnippetKind::Code => 0,
            ReadPackSnippetKind::Config => 1,
            ReadPackSnippetKind::Doc => 2,
        };

        a_rank
            .cmp(&b_rank)
            .then_with(|| {
                score_recall_snippet(params.question_tokens, b)
                    .cmp(&score_recall_snippet(params.question_tokens, a))
            })
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.start_line.cmp(&b.start_line))
            .then_with(|| a.end_line.cmp(&b.end_line))
    });

    merged.truncate(params.snippet_limit.max(1));
    *snippets = merged;
    Ok(())
}

struct GrepSnippetParams {
    file: Option<String>,
    file_pattern: Option<String>,
    before: usize,
    after: usize,
    max_hunks: usize,
    max_chars: usize,
    case_sensitive: bool,
    allow_secrets: bool,
}

fn recall_prefix_matches(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim().replace('\\', "/");
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }

    path == prefix || path.starts_with(&format!("{prefix}/"))
}

fn recall_path_allowed(path: &str, include_paths: &[String], exclude_paths: &[String]) -> bool {
    let path = path.replace('\\', "/");
    if exclude_paths
        .iter()
        .any(|prefix| !prefix.trim().is_empty() && recall_prefix_matches(&path, prefix))
    {
        return false;
    }

    if include_paths.is_empty() {
        return true;
    }

    include_paths
        .iter()
        .any(|prefix| !prefix.trim().is_empty() && recall_prefix_matches(&path, prefix))
}

fn scan_file_pattern_for_include_prefix(root: &Path, prefix: &str) -> Option<String> {
    let normalized = prefix.trim().replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    if normalized.is_empty() {
        return None;
    }

    if root.join(normalized).is_dir() {
        return Some(format!("{normalized}/"));
    }

    Some(normalized.to_string())
}

async fn snippets_from_grep(
    ctx: &ReadPackContext,
    pattern: &str,
    params: GrepSnippetParams,
) -> ToolResult<(Vec<ReadPackSnippet>, Option<String>)> {
    let max_hunks = params.max_hunks;
    let regex = RegexBuilder::new(pattern)
        .case_insensitive(!params.case_sensitive)
        .build()
        .map_err(|err| call_error("invalid_request", format!("Invalid regex: {err}")))?;
    let grep_request = GrepContextRequest {
        path: None,
        pattern: Some(pattern.to_string()),
        literal: Some(false),
        file: params.file,
        file_pattern: params.file_pattern,
        context: None,
        before: Some(params.before),
        after: Some(params.after),
        max_matches: Some(MAX_GREP_MATCHES.min(5_000)),
        max_hunks: Some(params.max_hunks),
        max_chars: Some(params.max_chars),
        case_sensitive: Some(params.case_sensitive),
        format: Some(ContentFormat::Plain),
        // Internal: these hunks are re-packed into read_pack snippets, so we can treat them as
        // "minimal" to maximize payload (grep_context's Facts mode reserves a lot of envelope
        // headroom that doesn't apply here).
        response_mode: Some(ResponseMode::Minimal),
        allow_secrets: Some(params.allow_secrets),
        cursor: None,
    };

    let result = compute_grep_context_result(
        &ctx.root,
        &ctx.root_display,
        &grep_request,
        &regex,
        GrepContextComputeOptions {
            case_sensitive: params.case_sensitive,
            before: params.before,
            after: params.after,
            max_matches: MAX_GREP_MATCHES.min(5_000),
            max_hunks: params.max_hunks,
            max_chars: params.max_chars,
            content_max_chars: super::router::grep_context::grep_context_content_budget(
                params.max_chars,
                ResponseMode::Minimal,
            ),
            resume_file: None,
            resume_line: 1,
        },
    )
    .await
    .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;

    let mut snippets = Vec::new();
    for hunk in result.hunks.iter().take(max_hunks) {
        snippets.push(ReadPackSnippet {
            file: hunk.file.clone(),
            start_line: hunk.start_line,
            end_line: hunk.end_line,
            content: hunk.content.clone(),
            kind: Some(snippet_kind_for_path(&hunk.file)),
            reason: Some(REASON_NEEDLE_GREP_HUNK.to_string()),
            next_cursor: None,
        });
    }
    Ok((snippets, result.next_cursor.clone()))
}

async fn snippets_from_grep_filtered(
    ctx: &ReadPackContext,
    pattern: &str,
    params: GrepSnippetParams,
    include_paths: &[String],
    exclude_paths: &[String],
    required_file_pattern: Option<&str>,
) -> ToolResult<(Vec<ReadPackSnippet>, Option<String>)> {
    let max_hunks = params.max_hunks.min(MAX_RECALL_SNIPPETS_PER_QUESTION);
    if let Some(file) = params.file.as_ref() {
        if !recall_path_allowed(file, include_paths, exclude_paths) {
            return Ok((Vec::new(), None));
        }
    }

    if include_paths.is_empty() {
        let (mut snippets, cursor) = snippets_from_grep(ctx, pattern, params).await?;
        snippets.retain(|snippet| {
            recall_path_allowed(&snippet.file, include_paths, exclude_paths)
                && ContextFinderService::matches_file_pattern(&snippet.file, required_file_pattern)
        });
        return Ok((snippets, cursor));
    }

    let mut out: Vec<ReadPackSnippet> = Vec::new();
    let mut seen: HashSet<(String, usize, usize)> = HashSet::new();

    for prefix in include_paths.iter().take(6) {
        let Some(scan_pattern) = scan_file_pattern_for_include_prefix(&ctx.root, prefix) else {
            continue;
        };

        let (snippets, _cursor) = snippets_from_grep(
            ctx,
            pattern,
            GrepSnippetParams {
                file: params.file.clone(),
                file_pattern: Some(scan_pattern),
                before: params.before,
                after: params.after,
                max_hunks: params.max_hunks,
                max_chars: params.max_chars,
                case_sensitive: params.case_sensitive,
                allow_secrets: params.allow_secrets,
            },
        )
        .await?;

        for snippet in snippets {
            if out.len() >= max_hunks {
                break;
            }
            if !recall_path_allowed(&snippet.file, include_paths, exclude_paths) {
                continue;
            }
            if !ContextFinderService::matches_file_pattern(&snippet.file, required_file_pattern) {
                continue;
            }
            let key = (snippet.file.clone(), snippet.start_line, snippet.end_line);
            if seen.insert(key) {
                out.push(snippet);
            }
        }

        if out.len() >= max_hunks {
            break;
        }
    }

    Ok((out, None))
}

#[derive(Clone, Copy, Debug)]
struct SnippetFromFileParams {
    around_line: Option<usize>,
    max_lines: usize,
    max_chars: usize,
    allow_secrets: bool,
}

async fn snippet_from_file(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    file: &str,
    params: SnippetFromFileParams,
    response_mode: ResponseMode,
) -> ToolResult<ReadPackSnippet> {
    if !params.allow_secrets && is_disallowed_memory_file(file) {
        return Err(call_error(
            "forbidden_file",
            "Refusing to read potential secret file via read_pack",
        ));
    }

    let start_line = params
        .around_line
        .map(|line| line.saturating_sub(params.max_lines / 3).max(1));
    let slice = compute_file_slice_result(
        &ctx.root,
        &ctx.root_display,
        &FileSliceRequest {
            path: None,
            file: Some(file.to_string()),
            start_line,
            max_lines: Some(params.max_lines),
            max_chars: Some(params.max_chars),
            format: None,
            response_mode: Some(ResponseMode::Facts),
            allow_secrets: Some(params.allow_secrets),
            cursor: None,
        },
    )
    .map_err(|err| call_error("internal", err))?;

    let kind = if response_mode == ResponseMode::Minimal {
        None
    } else {
        Some(snippet_kind_for_path(file))
    };
    let next_cursor = if response_mode == ResponseMode::Full {
        match slice.next_cursor.clone() {
            Some(cursor) => Some(compact_cursor_alias(service, cursor).await),
            None => None,
        }
    } else {
        None
    };
    Ok(ReadPackSnippet {
        file: slice.file.clone(),
        start_line: slice.start_line,
        end_line: slice.end_line,
        content: slice.content.clone(),
        kind,
        reason: Some(REASON_NEEDLE_FILE_SLICE.to_string()),
        next_cursor,
    })
}

fn parse_recall_regex_directive(question: &str) -> Option<String> {
    let q = question.trim();
    let lowered = q.to_ascii_lowercase();
    for prefix in ["re:", "regex:"] {
        if lowered.starts_with(prefix) {
            let rest = q[prefix.len()..].trim();
            if rest.is_empty() {
                return None;
            }
            return Some(rest.to_string());
        }
    }
    None
}

fn parse_recall_literal_directive(question: &str) -> Option<String> {
    let q = question.trim();
    let lowered = q.to_ascii_lowercase();
    for prefix in ["lit:", "literal:"] {
        if lowered.starts_with(prefix) {
            let rest = q[prefix.len()..].trim();
            if rest.is_empty() {
                return None;
            }
            return Some(rest.to_string());
        }
    }
    None
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum RecallQuestionMode {
    #[default]
    Auto,
    Fast,
    Deep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RecallQuestionPolicy {
    allow_semantic: bool,
}

fn recall_question_policy(
    mode: RecallQuestionMode,
    semantic_index_fresh: bool,
) -> RecallQuestionPolicy {
    let allow_semantic = match mode {
        RecallQuestionMode::Fast => false,
        RecallQuestionMode::Deep => true,
        RecallQuestionMode::Auto => semantic_index_fresh,
    };

    RecallQuestionPolicy { allow_semantic }
}

#[derive(Debug, Default)]
struct RecallQuestionDirectives {
    mode: RecallQuestionMode,
    snippet_limit: Option<usize>,
    grep_context: Option<usize>,
    include_paths: Vec<String>,
    exclude_paths: Vec<String>,
    file_pattern: Option<String>,
    file_ref: Option<(String, Option<usize>)>,
}

fn normalize_recall_directive_prefix(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let (token, _line) = parse_path_token(raw)?;
    let token = trim_utf8_bytes(&token, MAX_RECALL_FILTER_PATH_BYTES);
    if token.is_empty() || token == "." || token.starts_with('/') || token.contains("..") {
        return None;
    }
    Some(token)
}

fn normalize_recall_directive_pattern(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let token = raw.replace('\\', "/");
    let token = token.strip_prefix("./").unwrap_or(&token);
    if token.is_empty() || token.starts_with('/') || token.contains("..") {
        return None;
    }
    Some(trim_utf8_bytes(token, MAX_RECALL_FILTER_PATH_BYTES))
}

fn parse_duration_ms_token(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let lowered = raw.to_ascii_lowercase();
    if let Some(value) = lowered.strip_suffix("ms") {
        return value.trim().parse::<u64>().ok();
    }
    if let Some(value) = lowered.strip_suffix('s') {
        let secs = value.trim().parse::<u64>().ok()?;
        return secs.checked_mul(1_000);
    }

    lowered.parse::<u64>().ok()
}

fn parse_recall_question_directives(
    question: &str,
    root: &Path,
) -> (String, RecallQuestionDirectives) {
    const MAX_DIRECTIVE_PREFIXES: usize = 4;

    let mut directives = RecallQuestionDirectives::default();
    let mut remaining: Vec<&str> = Vec::new();

    for token in question.split_whitespace() {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }

        let lowered = token.to_ascii_lowercase();

        match lowered.as_str() {
            "fast" | "quick" | "grep" => {
                directives.mode = RecallQuestionMode::Fast;
                continue;
            }
            "deep" | "semantic" | "sem" | "index" => {
                directives.mode = RecallQuestionMode::Deep;
                continue;
            }
            _ => {}
        }

        if let Some(rest) = lowered
            .strip_prefix("index:")
            .or_else(|| lowered.strip_prefix("deep:"))
        {
            if parse_duration_ms_token(rest).is_some() {
                directives.mode = RecallQuestionMode::Deep;
                continue;
            }
        }

        if let Some(rest) = lowered
            .strip_prefix("k:")
            .or_else(|| lowered.strip_prefix("snips:"))
            .or_else(|| lowered.strip_prefix("top:"))
        {
            if let Ok(k) = rest.trim().parse::<usize>() {
                directives.snippet_limit = Some(k.clamp(1, MAX_RECALL_SNIPPETS_PER_QUESTION));
                continue;
            }
        }

        if let Some(rest) = lowered
            .strip_prefix("ctx:")
            .or_else(|| lowered.strip_prefix("context:"))
        {
            if let Ok(lines) = rest.trim().parse::<usize>() {
                directives.grep_context = Some(lines.clamp(0, 40));
                continue;
            }
        }

        let include_prefixes = ["in:", "scope:"];
        if include_prefixes.iter().any(|p| lowered.starts_with(p)) {
            if directives.include_paths.len() < MAX_DIRECTIVE_PREFIXES {
                let prefix_len = include_prefixes
                    .iter()
                    .find(|p| lowered.starts_with(*p))
                    .map(|p| p.len())
                    .unwrap_or(0);
                if let Some(prefix) =
                    normalize_recall_directive_prefix(token.get(prefix_len..).unwrap_or(""))
                {
                    directives.include_paths.push(prefix);
                }
            }
            continue;
        }

        let exclude_prefixes = ["not:", "out:", "exclude:"];
        if exclude_prefixes.iter().any(|p| lowered.starts_with(p)) {
            if directives.exclude_paths.len() < MAX_DIRECTIVE_PREFIXES {
                let prefix_len = exclude_prefixes
                    .iter()
                    .find(|p| lowered.starts_with(*p))
                    .map(|p| p.len())
                    .unwrap_or(0);
                if let Some(prefix) =
                    normalize_recall_directive_prefix(token.get(prefix_len..).unwrap_or(""))
                {
                    directives.exclude_paths.push(prefix);
                }
            }
            continue;
        }

        let pattern_prefixes = ["fp:", "glob:"];
        if pattern_prefixes.iter().any(|p| lowered.starts_with(p)) {
            let prefix_len = pattern_prefixes
                .iter()
                .find(|p| lowered.starts_with(*p))
                .map(|p| p.len())
                .unwrap_or(0);
            directives.file_pattern =
                normalize_recall_directive_pattern(token.get(prefix_len..).unwrap_or(""));
            continue;
        }

        let file_prefixes = ["file:", "open:"];
        if file_prefixes.iter().any(|p| lowered.starts_with(p)) {
            let prefix_len = file_prefixes
                .iter()
                .find(|p| lowered.starts_with(*p))
                .map(|p| p.len())
                .unwrap_or(0);
            let Some((candidate, line)) = parse_path_token(token.get(prefix_len..).unwrap_or(""))
            else {
                continue;
            };
            if is_disallowed_memory_file(&candidate) {
                continue;
            }
            if root.join(&candidate).is_file() {
                directives.file_ref = Some((candidate, line));
            }
            continue;
        }

        remaining.push(token);
    }

    let cleaned = remaining.join(" ").trim().to_string();
    (cleaned, directives)
}

fn merge_recall_prefix_lists(base: &[String], extra: &[String], max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for value in base.iter().chain(extra.iter()) {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if out.len() >= max {
            break;
        }
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    }

    out
}

fn build_semantic_query(question: &str, topics: Option<&Vec<String>>) -> String {
    let Some(topics) = topics else {
        return question.to_string();
    };
    if topics.is_empty() {
        return question.to_string();
    }

    let joined = topics.join(", ");
    format!("{question}\n\nTopics: {joined}")
}

async fn decode_recall_cursor(
    service: &ContextFinderService,
    cursor: &str,
) -> ToolResult<ReadPackRecallCursorV1> {
    let value: serde_json::Value = decode_cursor(cursor)
        .map_err(|err| call_error("invalid_cursor", format!("Invalid cursor: {err}")))?;

    if value.get("tool").and_then(Value::as_str) != Some("read_pack")
        || value.get("mode").and_then(Value::as_str) != Some("recall")
    {
        return Err(call_error("invalid_cursor", "Invalid cursor: wrong tool"));
    }

    let store_id = value.get("store_id").and_then(|v| v.as_u64());
    if let Some(store_id) = store_id {
        let Some(bytes) = service.state.cursor_store_get(store_id).await else {
            return Err(call_error(
                "invalid_cursor",
                "Invalid cursor: expired recall continuation",
            ));
        };
        return serde_json::from_slice::<ReadPackRecallCursorV1>(&bytes).map_err(|err| {
            call_error(
                "invalid_cursor",
                format!("Invalid cursor: stored continuation decode failed: {err}"),
            )
        });
    }

    serde_json::from_value::<ReadPackRecallCursorV1>(value).map_err(|err| {
        call_error(
            "invalid_cursor",
            format!("Invalid cursor: recall cursor decode failed: {err}"),
        )
    })
}

async fn handle_recall_intent(
    service: &ContextFinderService,
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    semantic_index_fresh: bool,
    sections: &mut Vec<ReadPackSection>,
    next_cursor_out: &mut Option<String>,
) -> ToolResult<()> {
    let (
        questions,
        topics,
        start_index,
        include_paths,
        exclude_paths,
        file_pattern,
        prefer_code,
        include_docs,
        allow_secrets,
    ) = if let Some(cursor) = trimmed_non_empty_str(request.cursor.as_deref()) {
        let overrides = request.ask.is_some()
            || request.questions.is_some()
            || request.topics.is_some()
            || request
                .include_paths
                .as_ref()
                .is_some_and(|p| p.iter().any(|p| !p.trim().is_empty()))
            || request
                .exclude_paths
                .as_ref()
                .is_some_and(|p| p.iter().any(|p| !p.trim().is_empty()))
            || trimmed_non_empty_str(request.file_pattern.as_deref()).is_some()
            || request.prefer_code.is_some()
            || request.include_docs.is_some()
            || request.allow_secrets.is_some();
        if overrides {
            return Err(call_error(
                "invalid_cursor",
                "Cursor continuation does not allow overriding recall parameters",
            ));
        }

        let decoded: ReadPackRecallCursorV1 = decode_recall_cursor(service, cursor).await?;
        if decoded.v != CURSOR_VERSION || decoded.tool != "read_pack" || decoded.mode != "recall" {
            return Err(call_error("invalid_cursor", "Invalid cursor: wrong tool"));
        }
        let expected_root_hash = cursor_fingerprint(&ctx.root_display);
        let expected_root_fingerprint = root_fingerprint(&ctx.root_display);
        if let Some(hash) = decoded.root_hash {
            if hash != expected_root_hash {
                return Err(invalid_cursor_with_meta_details(
                    "Invalid cursor: different root",
                    ToolMeta {
                        root_fingerprint: Some(expected_root_fingerprint),
                        ..ToolMeta::default()
                    },
                    json!({
                        "expected_root_fingerprint": expected_root_fingerprint,
                        "cursor_root_fingerprint": Some(hash),
                    }),
                ));
            }
        } else if decoded.root.as_deref() != Some(ctx.root_display.as_str()) {
            let cursor_root_fingerprint = decoded.root.as_deref().map(root_fingerprint);
            return Err(invalid_cursor_with_meta_details(
                "Invalid cursor: different root",
                ToolMeta {
                    root_fingerprint: Some(expected_root_fingerprint),
                    ..ToolMeta::default()
                },
                json!({
                    "expected_root_fingerprint": expected_root_fingerprint,
                    "cursor_root_fingerprint": cursor_root_fingerprint,
                }),
            ));
        }

        (
            decoded.questions,
            decoded.topics,
            decoded.next_question_index,
            decoded.include_paths,
            decoded.exclude_paths,
            decoded.file_pattern,
            decoded.prefer_code,
            decoded.include_docs,
            decoded.allow_secrets,
        )
    } else {
        (
            normalize_questions(request),
            normalize_topics(request),
            0,
            normalize_path_prefix_list(request.include_paths.as_ref()),
            normalize_path_prefix_list(request.exclude_paths.as_ref()),
            normalize_optional_pattern(request.file_pattern.as_deref()),
            request.prefer_code,
            request.include_docs,
            request.allow_secrets.unwrap_or(false),
        )
    };

    if questions.is_empty() {
        return Err(call_error(
            "missing_field",
            "Error: ask or questions is required for intent=recall",
        ));
    }

    let facts_snapshot = sections
        .iter()
        .find_map(|section| match section {
            ReadPackSection::ProjectFacts { result } => Some(result.clone()),
            _ => None,
        })
        .unwrap_or_else(|| compute_project_facts(&ctx.root));

    // Recall is a tight-loop tool and must stay cheap by default.
    //
    // Agent-native behavior: do not expose indexing knobs. Semantic retrieval is used only when
    // the index is already fresh, or when the user explicitly tags a question as `deep`.

    let remaining_questions = questions.len().saturating_sub(start_index).max(1);
    // Memory-UX heuristic: try to answer *more* questions per call by default, but keep snippets
    // small/dry so we fit under budget. This makes recall feel like "project memory" instead of
    // "a sequence of grep calls".
    //
    // We reserve a small slice for the facts section so the questions don't starve the front of
    // the page under mid budgets.
    let reserve_for_facts = match ctx.inner_max_chars {
        0..=2_000 => 260,
        2_001..=6_000 => 420,
        6_001..=12_000 => 650,
        _ => 900,
    };
    let recall_budget_pool = ctx
        .inner_max_chars
        .saturating_sub(reserve_for_facts)
        .max(80)
        .min(ctx.inner_max_chars);

    // Target ~1.4k chars per question under `.context` output. This is intentionally conservative:
    // we'd rather answer more questions with smaller snippets and let the agent "zoom in" with
    // cursor/deep mode.
    let target_per_question = 1_400usize;
    let min_per_question = 650usize;

    let max_questions_by_target = (recall_budget_pool / target_per_question).clamp(1, 8);
    let max_questions_by_min = (recall_budget_pool / min_per_question).max(1);
    let max_questions_this_call = max_questions_by_target
        .min(max_questions_by_min)
        .min(remaining_questions);

    let per_question_budget = recall_budget_pool
        .saturating_div(max_questions_this_call.max(1))
        .max(80);

    // Under smaller per-question budgets, prefer fewer, more informative snippets.
    let default_snippets_auto = if per_question_budget < 1_500 {
        1
    } else if per_question_budget < 3_200 {
        2
    } else {
        DEFAULT_RECALL_SNIPPETS_PER_QUESTION
    };
    let default_snippets_fast = if per_question_budget < 1_500 { 1 } else { 2 };

    let mut used_files: HashSet<String> = {
        // Per-session working set: avoid repeating the same anchor files across multiple recall
        // calls in one agent session.
        let session = service.session.lock().await;
        session.seen_snippet_files_set_snapshot()
    };
    let mut processed = 0usize;
    let mut next_index = None;

    for (offset, question) in questions.iter().enumerate().skip(start_index) {
        let mut snippets: Vec<ReadPackSnippet> = Vec::new();

        let (clean_question, directives) = parse_recall_question_directives(question, &ctx.root);
        let clean_question = if clean_question.is_empty() {
            question.clone()
        } else {
            clean_question
        };
        let user_directive = parse_recall_regex_directive(&clean_question).is_some()
            || parse_recall_literal_directive(&clean_question).is_some();
        let structural_intent = if user_directive {
            None
        } else {
            recall_structural_intent(&clean_question)
        };
        let ops = ops_intent(&clean_question);
        let is_ops = ops.is_some();
        let question_tokens = recall_question_tokens(&clean_question);
        let docs_intent = QueryClassifier::is_docs_intent(&clean_question);
        let effective_prefer_code = prefer_code.unwrap_or(!docs_intent);

        let question_mode = directives.mode;
        let base_snippet_limit = match question_mode {
            RecallQuestionMode::Fast => default_snippets_fast,
            RecallQuestionMode::Deep => MAX_RECALL_SNIPPETS_PER_QUESTION,
            RecallQuestionMode::Auto => default_snippets_auto,
        };
        let snippet_limit = directives
            .snippet_limit
            .unwrap_or(base_snippet_limit)
            .clamp(1, MAX_RECALL_SNIPPETS_PER_QUESTION);
        let grep_context_lines = directives.grep_context.unwrap_or(12);

        let snippet_max_chars = per_question_budget
            .saturating_div(snippet_limit.max(1))
            .clamp(40, 4_000)
            .min(ctx.inner_max_chars);
        let snippet_max_chars = match question_mode {
            RecallQuestionMode::Deep => snippet_max_chars,
            _ => snippet_max_chars.min(1_200),
        };
        let snippet_max_lines = if snippet_max_chars < 600 {
            60
        } else if snippet_max_chars < 1_200 {
            90
        } else {
            120
        };

        let policy = recall_question_policy(question_mode, semantic_index_fresh);
        let allow_semantic = policy.allow_semantic;

        let effective_include_paths = merge_recall_prefix_lists(
            &include_paths,
            &directives.include_paths,
            MAX_RECALL_FILTER_PATHS,
        );
        let effective_exclude_paths = merge_recall_prefix_lists(
            &exclude_paths,
            &directives.exclude_paths,
            MAX_RECALL_FILTER_PATHS,
        );
        let effective_file_pattern = directives
            .file_pattern
            .clone()
            .or_else(|| file_pattern.clone());

        let explicit_file_ref = directives.file_ref.clone();
        let detected_file_ref =
            extract_existing_file_ref(&clean_question, &ctx.root, allow_secrets);
        let file_ref = explicit_file_ref.or(detected_file_ref);

        if let Some((file, line)) = file_ref {
            if let Ok(snippet) = snippet_from_file(
                service,
                ctx,
                &file,
                SnippetFromFileParams {
                    around_line: line,
                    max_lines: snippet_max_lines,
                    max_chars: snippet_max_chars,
                    allow_secrets,
                },
                response_mode,
            )
            .await
            {
                snippets.push(snippet);
            }
        }

        if snippets.is_empty() {
            if let Some(structural_intent) = structural_intent {
                let candidates =
                    recall_structural_candidates(structural_intent, &ctx.root, &facts_snapshot);
                for file in candidates.into_iter().take(32) {
                    if !recall_path_allowed(
                        &file,
                        &effective_include_paths,
                        &effective_exclude_paths,
                    ) {
                        continue;
                    }
                    if !ContextFinderService::matches_file_pattern(
                        &file,
                        effective_file_pattern.as_deref(),
                    ) {
                        continue;
                    }

                    let kind = snippet_kind_for_path(&file);
                    let anchor = best_anchor_line_for_kind(&ctx.root, &file, kind);

                    if let Ok(snippet) = snippet_from_file(
                        service,
                        ctx,
                        &file,
                        SnippetFromFileParams {
                            around_line: anchor,
                            max_lines: snippet_max_lines,
                            max_chars: snippet_max_chars,
                            allow_secrets,
                        },
                        response_mode,
                    )
                    .await
                    {
                        snippets.push(snippet);
                    }

                    if snippets.len() >= snippet_limit {
                        break;
                    }
                }
            }
        }

        if snippets.is_empty() {
            if let Some(regex) = parse_recall_regex_directive(&clean_question) {
                if let Ok((found, _)) = snippets_from_grep_filtered(
                    ctx,
                    &regex,
                    GrepSnippetParams {
                        file: None,
                        file_pattern: effective_file_pattern.clone(),
                        before: grep_context_lines,
                        after: grep_context_lines,
                        max_hunks: snippet_limit,
                        max_chars: snippet_max_chars,
                        case_sensitive: true,
                        allow_secrets,
                    },
                    &effective_include_paths,
                    &effective_exclude_paths,
                    effective_file_pattern.as_deref(),
                )
                .await
                {
                    snippets = found;
                } else {
                    let escaped = regex::escape(&regex);
                    if let Ok((found, _)) = snippets_from_grep_filtered(
                        ctx,
                        &escaped,
                        GrepSnippetParams {
                            file: None,
                            file_pattern: effective_file_pattern.clone(),
                            before: grep_context_lines,
                            after: grep_context_lines,
                            max_hunks: snippet_limit,
                            max_chars: snippet_max_chars,
                            case_sensitive: false,
                            allow_secrets,
                        },
                        &effective_include_paths,
                        &effective_exclude_paths,
                        effective_file_pattern.as_deref(),
                    )
                    .await
                    {
                        snippets = found;
                    }
                }
            }
        }

        if snippets.is_empty() {
            if let Some(literal) = parse_recall_literal_directive(&clean_question) {
                let escaped = regex::escape(&literal);
                if let Ok((found, _)) = snippets_from_grep_filtered(
                    ctx,
                    &escaped,
                    GrepSnippetParams {
                        file: None,
                        file_pattern: effective_file_pattern.clone(),
                        before: grep_context_lines,
                        after: grep_context_lines,
                        max_hunks: snippet_limit,
                        max_chars: snippet_max_chars,
                        case_sensitive: false,
                        allow_secrets,
                    },
                    &effective_include_paths,
                    &effective_exclude_paths,
                    effective_file_pattern.as_deref(),
                )
                .await
                {
                    snippets = found;
                }
            }
        }

        if snippets.is_empty() {
            if let Some(intent) = ops {
                let pattern = ops_grep_pattern(intent);
                let candidates = collect_ops_file_candidates(&ctx.root);

                // Scan a bounded set of likely "commands live here" files and rerank matches by
                // overlap with the question. This avoids getting stuck on the first generic
                // `cargo run` mention when the question is actually about a more specific workflow
                // (e.g., golden snapshots).
                let mut found_snippets: Vec<ReadPackSnippet> = Vec::new();
                for file in candidates.into_iter().take(24) {
                    if !recall_path_allowed(
                        &file,
                        &effective_include_paths,
                        &effective_exclude_paths,
                    ) {
                        continue;
                    }
                    if !ContextFinderService::matches_file_pattern(
                        &file,
                        effective_file_pattern.as_deref(),
                    ) {
                        continue;
                    }
                    let Ok((mut found, _)) = snippets_from_grep(
                        ctx,
                        pattern,
                        GrepSnippetParams {
                            file: Some(file.clone()),
                            file_pattern: None,
                            before: grep_context_lines.min(20),
                            after: grep_context_lines.min(20),
                            max_hunks: snippet_limit,
                            max_chars: snippet_max_chars,
                            case_sensitive: false,
                            allow_secrets,
                        },
                    )
                    .await
                    else {
                        continue;
                    };
                    found_snippets.append(&mut found);
                    if found_snippets.len() >= snippet_limit.saturating_mul(3) {
                        break;
                    }
                }

                if !found_snippets.is_empty() {
                    found_snippets.sort_by(|a, b| {
                        let a_score = score_recall_snippet(&question_tokens, a);
                        let b_score = score_recall_snippet(&question_tokens, b);
                        b_score
                            .cmp(&a_score)
                            .then_with(|| {
                                ops_candidate_score(&b.file).cmp(&ops_candidate_score(&a.file))
                            })
                            .then_with(|| a.file.cmp(&b.file))
                            .then_with(|| a.start_line.cmp(&b.start_line))
                            .then_with(|| a.end_line.cmp(&b.end_line))
                    });
                    found_snippets.truncate(snippet_limit);
                    snippets = found_snippets;
                }

                // If there are no concrete command matches, fall back to a deterministic
                // anchor-based doc snippet instead of grepping the entire repo.
                if snippets.is_empty() {
                    let candidates = collect_ops_file_candidates(&ctx.root);
                    for file in candidates.into_iter().take(10) {
                        if !recall_path_allowed(
                            &file,
                            &effective_include_paths,
                            &effective_exclude_paths,
                        ) {
                            continue;
                        }
                        if !ContextFinderService::matches_file_pattern(
                            &file,
                            effective_file_pattern.as_deref(),
                        ) {
                            continue;
                        }
                        let kind = snippet_kind_for_path(&file);
                        if kind == ReadPackSnippetKind::Code {
                            continue;
                        }
                        let Some(anchor) = best_anchor_line_for_kind(&ctx.root, &file, kind) else {
                            continue;
                        };
                        if let Ok(snippet) = snippet_from_file(
                            service,
                            ctx,
                            &file,
                            SnippetFromFileParams {
                                around_line: Some(anchor),
                                max_lines: snippet_max_lines,
                                max_chars: snippet_max_chars,
                                allow_secrets,
                            },
                            response_mode,
                        )
                        .await
                        {
                            snippets.push(snippet);
                            break;
                        }
                    }
                }
            }
        }

        if snippets.is_empty() {
            // Best-effort: use semantic search if an index already exists; otherwise fall back to grep.
            let avoid_semantic_for_structural =
                structural_intent.is_some() && question_mode != RecallQuestionMode::Deep;
            if allow_semantic
                && !avoid_semantic_for_structural
                && (!is_ops || question_mode == RecallQuestionMode::Deep)
            {
                let tool_result = super::router::context_pack::context_pack(
                    service,
                    ContextPackRequest {
                        path: Some(ctx.root_display.clone()),
                        query: build_semantic_query(&clean_question, topics.as_ref()),
                        language: None,
                        strategy: None,
                        limit: Some(snippet_limit),
                        max_chars: Some(
                            snippet_max_chars
                                .saturating_mul(snippet_limit)
                                .saturating_mul(2)
                                .clamp(1_000, 20_000),
                        ),
                        include_paths: if effective_include_paths.is_empty() {
                            None
                        } else {
                            Some(effective_include_paths.clone())
                        },
                        exclude_paths: if effective_exclude_paths.is_empty() {
                            None
                        } else {
                            Some(effective_exclude_paths.clone())
                        },
                        file_pattern: effective_file_pattern.clone(),
                        max_related_per_primary: Some(1),
                        include_docs,
                        prefer_code,
                        related_mode: Some("focus".to_string()),
                        response_mode: Some(ResponseMode::Minimal),
                        trace: Some(false),
                        auto_index: None,
                        auto_index_budget_ms: None,
                    },
                )
                .await;

                if let Ok(tool_result) = tool_result {
                    if tool_result.is_error != Some(true) {
                        if let Some(value) = tool_result.structured_content.clone() {
                            if let Some(items) = value.get("items").and_then(|v| v.as_array()) {
                                for item in items.iter().take(snippet_limit) {
                                    let Some(file) = item.get("file").and_then(|v| v.as_str())
                                    else {
                                        continue;
                                    };
                                    let Some(content) =
                                        item.get("content").and_then(|v| v.as_str())
                                    else {
                                        continue;
                                    };
                                    let start_line = item
                                        .get("start_line")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(1)
                                        as usize;
                                    let start_line_u64 = start_line as u64;
                                    let end_line = item
                                        .get("end_line")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(start_line_u64)
                                        as usize;
                                    if !allow_secrets && is_disallowed_memory_file(file) {
                                        continue;
                                    }
                                    snippets.push(ReadPackSnippet {
                                        file: file.to_string(),
                                        start_line,
                                        end_line,
                                        content: trim_chars(content, snippet_max_chars),
                                        kind: if response_mode == ResponseMode::Minimal {
                                            None
                                        } else {
                                            Some(snippet_kind_for_path(file))
                                        },
                                        reason: Some(REASON_HALO_CONTEXT_PACK_PRIMARY.to_string()),
                                        next_cursor: None,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        if snippets.is_empty() && !is_ops {
            if let Some(keyword) = best_keyword_pattern(&clean_question) {
                if let Ok((found, _)) = snippets_from_grep_filtered(
                    ctx,
                    &keyword,
                    GrepSnippetParams {
                        file: None,
                        file_pattern: effective_file_pattern.clone(),
                        before: grep_context_lines,
                        after: grep_context_lines,
                        max_hunks: snippet_limit,
                        max_chars: snippet_max_chars,
                        case_sensitive: false,
                        allow_secrets,
                    },
                    &effective_include_paths,
                    &effective_exclude_paths,
                    effective_file_pattern.as_deref(),
                )
                .await
                {
                    snippets = found;
                }
            }
        }

        if effective_prefer_code
            && structural_intent.is_none()
            && !is_ops
            && !user_directive
            && !docs_intent
            && !snippets.is_empty()
            && !recall_has_code_snippet(&snippets)
        {
            let _ = recall_upgrade_to_code_snippets(
                RecallCodeUpgradeParams {
                    ctx,
                    facts_snapshot: &facts_snapshot,
                    question_tokens: &question_tokens,
                    snippet_limit,
                    snippet_max_chars,
                    grep_context_lines,
                    include_paths: &effective_include_paths,
                    exclude_paths: &effective_exclude_paths,
                    file_pattern: effective_file_pattern.as_deref(),
                    allow_secrets,
                },
                &mut snippets,
            )
            .await;
        }

        if snippets.len() > snippet_limit {
            snippets.truncate(snippet_limit);
        }

        // Global de-dupe: prefer covering *more files* (breadth) when answering multiple
        // questions in one call. This prevents "README spam" from consuming the entire budget.
        if snippets.len() > 1 {
            let mut unique: Vec<ReadPackSnippet> = Vec::new();
            let mut duplicates: Vec<ReadPackSnippet> = Vec::new();
            for snippet in snippets {
                if used_files.insert(snippet.file.clone()) {
                    unique.push(snippet);
                } else {
                    duplicates.push(snippet);
                }
            }
            if unique.is_empty() {
                if let Some(first) = duplicates.into_iter().next() {
                    unique.push(first);
                }
            }
            snippets = unique;
        } else if let Some(snippet) = snippets.first() {
            used_files.insert(snippet.file.clone());
        }

        sections.push(ReadPackSection::Recall {
            result: ReadPackRecallResult {
                question: question.clone(),
                snippets,
            },
        });
        processed += 1;

        // Pagination guard: keep recall bounded, while letting larger budgets answer more questions.
        if processed >= max_questions_this_call {
            next_index = Some(offset + 1);
            break;
        }
    }

    if let Some(next_question_index) = next_index {
        let remaining_questions: Vec<String> = questions
            .iter()
            .skip(next_question_index)
            .cloned()
            .collect();
        if remaining_questions.is_empty() {
            return Ok(());
        }
        let cursor = ReadPackRecallCursorV1 {
            v: CURSOR_VERSION,
            tool: "read_pack".to_string(),
            mode: "recall".to_string(),
            root: Some(ctx.root_display.clone()),
            root_hash: Some(cursor_fingerprint(&ctx.root_display)),
            max_chars: Some(ctx.max_chars),
            response_mode: Some(response_mode),
            questions: remaining_questions,
            topics,
            include_paths,
            exclude_paths,
            file_pattern,
            prefer_code,
            include_docs,
            allow_secrets,
            next_question_index: 0,
        };

        // Try to keep cursors inline (stateless) when small; otherwise store the full continuation
        // server-side and return a tiny cursor token (agent-friendly, avoids blowing context).
        if let Ok(token) = encode_cursor(&cursor) {
            if token.len() <= MAX_RECALL_INLINE_CURSOR_CHARS {
                *next_cursor_out = Some(compact_cursor_alias(service, token).await);
                return Ok(());
            }
        }

        let stored_bytes =
            serde_json::to_vec(&cursor).map_err(|err| call_error("internal", err.to_string()))?;
        let store_id = service.state.cursor_store_put(stored_bytes).await;
        let stored_cursor = ReadPackRecallCursorStoredV1 {
            v: CURSOR_VERSION,
            tool: "read_pack".to_string(),
            mode: "recall".to_string(),
            root: Some(ctx.root_display.clone()),
            root_hash: Some(cursor_fingerprint(&ctx.root_display)),
            max_chars: Some(ctx.max_chars),
            response_mode: Some(response_mode),
            store_id,
        };
        if let Ok(token) = encode_cursor(&stored_cursor) {
            *next_cursor_out = Some(compact_cursor_alias(service, token).await);
        }
    }

    Ok(())
}

fn truncate_to_chars(input: &str, max_chars: usize) -> String {
    let max_chars = max_chars.max(1);
    let mut cut_byte = input.len();
    for (seen, (idx, _)) in input.char_indices().enumerate() {
        if seen == max_chars {
            cut_byte = idx;
            break;
        }
    }
    input[..cut_byte].to_string()
}

fn read_pack_intent_label(intent: ReadPackIntent) -> &'static str {
    match intent {
        ReadPackIntent::Auto => "auto",
        ReadPackIntent::File => "file",
        ReadPackIntent::Grep => "grep",
        ReadPackIntent::Query => "query",
        ReadPackIntent::Onboarding => "onboarding",
        ReadPackIntent::Memory => "memory",
        ReadPackIntent::Recall => "recall",
    }
}

fn render_read_pack_context_doc(result: &ReadPackResult, response_mode: ResponseMode) -> String {
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

    for section in &result.sections {
        let ReadPackSection::ProjectFacts { result: facts } = section else {
            continue;
        };
        if !facts.ecosystems.is_empty() {
            doc.push_note(&format!("ecosystems: {}", facts.ecosystems.join(", ")));
        }
        if !facts.build_tools.is_empty() {
            doc.push_note(&format!("build_tools: {}", facts.build_tools.join(", ")));
        }
        if !facts.ci.is_empty() {
            doc.push_note(&format!("ci: {}", facts.ci.join(", ")));
        }
        if !facts.contracts.is_empty() {
            doc.push_note(&format!("contracts: {}", facts.contracts.join(", ")));
        }
        if !facts.key_dirs.is_empty() {
            doc.push_note(&format!("key_dirs: {}", facts.key_dirs.join(", ")));
        }
        if !facts.modules.is_empty() {
            doc.push_note(&format!("modules: {}", facts.modules.join(", ")));
        }
        if !facts.entry_points.is_empty() {
            doc.push_note(&format!("entry_points: {}", facts.entry_points.join(", ")));
        }
        if !facts.key_configs.is_empty() {
            doc.push_note(&format!("key_configs: {}", facts.key_configs.join(", ")));
        }
        break;
    }

    for section in &result.sections {
        match section {
            ReadPackSection::ProjectFacts { .. } => {}
            ReadPackSection::ExternalMemory { result: memory } => {
                doc.push_note(&format!(
                    "external_memory: source={} hits={}",
                    memory.source,
                    memory.hits.len()
                ));
                for hit in &memory.hits {
                    let title = hit.title.as_deref().unwrap_or("");
                    if title.trim().is_empty() {
                        doc.push_note(&format!(
                            "memory_hit: [{}] score={:.3}",
                            hit.kind, hit.score
                        ));
                    } else {
                        doc.push_note(&format!(
                            "memory_hit: [{}] {} (score={:.3})",
                            hit.kind, title, hit.score
                        ));
                    }
                    if response_mode != ResponseMode::Minimal && !hit.excerpt.trim().is_empty() {
                        doc.push_block_smart(&hit.excerpt);
                        doc.push_blank();
                    }
                }
            }
            ReadPackSection::Snippet { result: snippet } => {
                let label = match snippet.kind {
                    Some(ReadPackSnippetKind::Code) => Some("code"),
                    Some(ReadPackSnippetKind::Doc) => Some("doc"),
                    Some(ReadPackSnippetKind::Config) => Some("config"),
                    None => None,
                };
                doc.push_ref_header(&snippet.file, snippet.start_line, label);
                if response_mode == ResponseMode::Full {
                    if let Some(reason) = snippet
                        .reason
                        .as_deref()
                        .filter(|reason| !reason.trim().is_empty())
                    {
                        doc.push_note(&format!("reason: {reason}"));
                    }
                }
                doc.push_block_smart(&snippet.content);
                doc.push_blank();
            }
            ReadPackSection::Recall { result: recall } => {
                doc.push_note(&format!("recall: {}", recall.question));
                for snippet in &recall.snippets {
                    let label = match snippet.kind {
                        Some(ReadPackSnippetKind::Code) => Some("code"),
                        Some(ReadPackSnippetKind::Doc) => Some("doc"),
                        Some(ReadPackSnippetKind::Config) => Some("config"),
                        None => None,
                    };
                    doc.push_ref_header(&snippet.file, snippet.start_line, label);
                    if response_mode == ResponseMode::Full {
                        if let Some(reason) = snippet
                            .reason
                            .as_deref()
                            .filter(|reason| !reason.trim().is_empty())
                        {
                            doc.push_note(&format!("reason: {reason}"));
                        }
                    }
                    doc.push_block_smart(&snippet.content);
                    doc.push_blank();
                }
            }
            ReadPackSection::FileSlice { result: slice } => {
                doc.push_ref_header(&slice.file, slice.start_line, Some("file slice"));
                doc.push_block_smart(&slice.content);
                doc.push_blank();
            }
            ReadPackSection::GrepContext { result: grep } => {
                doc.push_note(&format!("grep: pattern={}", grep.pattern));
                for hunk in &grep.hunks {
                    doc.push_ref_header(&hunk.file, hunk.start_line, Some("grep hunk"));
                    doc.push_block_smart(&hunk.content);
                    doc.push_blank();
                }
            }
            ReadPackSection::Overview { result: overview } => {
                doc.push_note(&format!(
                    "overview: {} files={} chunks={} lines={} graph(nodes={} edges={})",
                    overview.project.name,
                    overview.project.files,
                    overview.project.chunks,
                    overview.project.lines,
                    overview.graph_stats.nodes,
                    overview.graph_stats.edges
                ));

                if response_mode != ResponseMode::Minimal {
                    if !overview.entry_points.is_empty() {
                        doc.push_note("entry_points:");
                        for ep in overview.entry_points.iter().take(6) {
                            doc.push_line(&format!(" - {ep}"));
                        }
                        if overview.entry_points.len() > 6 {
                            doc.push_line(&format!(
                                " - … (showing 6 of {})",
                                overview.entry_points.len()
                            ));
                        }
                    }
                    if !overview.layers.is_empty() {
                        doc.push_note("layers:");
                        for layer in overview.layers.iter().take(6) {
                            doc.push_line(&format!(
                                " - {} (files={}) — {}",
                                layer.name, layer.files, layer.role
                            ));
                        }
                        if overview.layers.len() > 6 {
                            doc.push_line(&format!(
                                " - … (showing 6 of {})",
                                overview.layers.len()
                            ));
                        }
                    }
                    if !overview.key_types.is_empty() {
                        doc.push_note("key_types:");
                        for ty in overview.key_types.iter().take(6) {
                            doc.push_line(&format!(
                                " - {} ({}) @ {} — coupling={}",
                                ty.name, ty.kind, ty.file, ty.coupling
                            ));
                        }
                        if overview.key_types.len() > 6 {
                            doc.push_line(&format!(
                                " - … (showing 6 of {})",
                                overview.key_types.len()
                            ));
                        }
                    }
                }

                doc.push_blank();
            }
            ReadPackSection::ContextPack { result: pack_value } => {
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
                                doc.push_ref_header(
                                    &item.file,
                                    item.start_line,
                                    Some(item.role.as_str()),
                                );
                                if let Some(symbol) = item.symbol.as_deref() {
                                    doc.push_note(&format!(
                                        "symbol={} score={:.3}",
                                        symbol, item.score
                                    ));
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
            ReadPackSection::RepoOnboardingPack { result: pack } => {
                doc.push_note(&format!(
                    "repo_onboarding_pack: docs={} omitted={} truncated={}",
                    pack.docs.len(),
                    pack.omitted_doc_paths.len(),
                    pack.budget.truncated
                ));
                if let Some(reason) = pack.docs_reason.as_ref() {
                    if response_mode == ResponseMode::Full {
                        doc.push_note(&format!("docs_reason={reason:?}"));
                    }
                }

                if response_mode != ResponseMode::Minimal {
                    doc.push_note(&format!(
                        "map: dirs={} truncated={}",
                        pack.map.directories.len(),
                        pack.map.truncated
                    ));
                }

                for doc_slice in &pack.docs {
                    doc.push_ref_header(&doc_slice.file, doc_slice.start_line, Some("doc slice"));
                    doc.push_block_smart(&doc_slice.content);
                    doc.push_blank();
                }

                if !pack.omitted_doc_paths.is_empty() {
                    doc.push_note(&format!("omitted_docs: {}", pack.omitted_doc_paths.len()));
                    for path in pack.omitted_doc_paths.iter().take(10) {
                        doc.push_line(&format!(" - {path}"));
                    }
                    if pack.omitted_doc_paths.len() > 10 {
                        doc.push_line(&format!(
                            " - … (showing 10 of {})",
                            pack.omitted_doc_paths.len()
                        ));
                    }
                    doc.push_blank();
                }

                if response_mode == ResponseMode::Full && !pack.next_actions.is_empty() {
                    doc.push_note("repo_onboarding_pack next_actions:");
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
            }
        }
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

/// Build a one-call semantic reading pack (file slice / grep context / context pack / onboarding).
pub(in crate::tools::dispatch) async fn read_pack(
    service: &ContextFinderService,
    mut request: ReadPackRequest,
) -> Result<CallToolResult, McpError> {
    // Expand compact cursor aliases early so routing and cursor-only continuation work.
    // Without this, `resolve_intent` would attempt to decode a non-base64 cursor alias directly.
    if let Some(cursor) = request.cursor.as_deref() {
        match expand_cursor_alias(service, cursor).await {
            Ok(expanded) => request.cursor = Some(expanded),
            Err(message) => {
                let meta = meta_for_request(service, request.path.as_deref()).await;
                return Ok(attach_meta(call_error("invalid_cursor", message), meta));
            }
        }
    }

    // Cursor-only continuation: if the caller didn't pass `path`, we can fall back to the cursor's
    // embedded root *only when the current session has no established root*.
    //
    // This is a safety boundary for multi-agent / multi-project usage: if a session already has a
    // default root (from a previous call), we refuse to silently switch projects based on a cursor
    // token that might have been copy/pasted or mixed across concurrent agent sessions.
    if trimmed_non_empty_str(request.path.as_deref()).is_none() {
        if let Some(cursor) = request.cursor.as_deref() {
            if let Ok(value) = decode_cursor::<Value>(cursor) {
                if let Some(root) = value.get("root").and_then(Value::as_str) {
                    let cursor_root = root.trim();
                    if !cursor_root.is_empty() {
                        let session_root_display = { service.session.lock().await.root_display() };
                        if let Some(session_root_display) = session_root_display {
                            if session_root_display != cursor_root {
                                let message = "Invalid cursor: cursor refers to a different project root than the current session; pass `path` to switch projects."
                                    .to_string();
                                let meta = ToolMeta {
                                    root_fingerprint: Some(root_fingerprint(&session_root_display)),
                                    ..ToolMeta::default()
                                };
                                return Ok(attach_meta(
                                    call_error("invalid_cursor", message),
                                    meta,
                                ));
                            }
                        } else {
                            request.path = Some(cursor_root.to_string());
                        }
                    }
                }
            }
        }
    }

    let mut hints: Vec<String> = Vec::new();
    if let Some(file) = request.file.as_deref() {
        hints.push(file.to_string());
    }
    let (root, root_display) = match service
        .resolve_root_with_hints(request.path.as_deref(), &hints)
        .await
    {
        Ok(value) => value,
        Err(message) => {
            return Ok(invalid_request_with_meta(
                message,
                ToolMeta::default(),
                None,
                Vec::new(),
            ))
        }
    };
    let base_meta = service.tool_meta(&root).await;

    // Cursor-only continuation should preserve caller-selected budgets and response mode.
    // Without this, a continuation can silently jump back to defaults (e.g. max_chars=20k), which
    // is catastrophic for an agent’s context window.
    if let Some(cursor) = request.cursor.as_deref() {
        match decode_cursor::<Value>(cursor) {
            Ok(value) => {
                if request.max_chars.is_none() {
                    if let Some(n) = value.get("max_chars").and_then(Value::as_u64) {
                        if n > 0 {
                            request.max_chars = Some(n as usize);
                        }
                    }
                }
                if request.response_mode.is_none() {
                    if let Some(mode_value) = value.get("response_mode") {
                        if let Ok(mode) = serde_json::from_value::<ResponseMode>(mode_value.clone())
                        {
                            request.response_mode = Some(mode);
                        }
                    }
                }
            }
            Err(err) => {
                return Ok(attach_meta(
                    call_error("invalid_cursor", format!("Invalid cursor: {err}")),
                    base_meta.clone(),
                ))
            }
        }
    }

    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let ctx = match build_context(&request, root, root_display) {
        Ok(value) => value,
        Err(result) => return Ok(attach_meta(result, base_meta.clone())),
    };
    let intent = match resolve_intent(&request) {
        Ok(value) => value,
        Err(result) => return Ok(attach_meta(result, base_meta.clone())),
    };

    let timeout_ms = request
        .timeout_ms
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(1_000, MAX_TIMEOUT_MS);
    let meta = match intent {
        ReadPackIntent::Query => {
            // Query intent is the "semantic default" surface: keep it fresh when possible.
            service
                .tool_meta_with_auto_index(&ctx.root, AutoIndexPolicy::semantic_default())
                .await
        }
        _ => base_meta.clone(),
    };
    // Low-noise default: keep the response mostly project content. Freshness/index diagnostics are
    // opt-in via `response_mode=full`.
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

    let mut sections: Vec<ReadPackSection> = Vec::new();
    let mut next_actions: Vec<ReadPackNextAction> = Vec::new();
    let mut next_cursor: Option<String> = None;

    let facts = service
        .state
        .project_facts_cache_get(&ctx.root)
        .await
        .unwrap_or_else(|| compute_project_facts(&ctx.root));
    service
        .state
        .project_facts_cache_put(&ctx.root, facts.clone())
        .await;
    let facts = trim_project_facts_for_budget(facts, &ctx, response_mode);
    sections.push(ReadPackSection::ProjectFacts {
        result: facts.clone(),
    });

    let handler_future = async {
        match intent {
            ReadPackIntent::Auto => unreachable!("auto intent resolved above"),
            ReadPackIntent::File => {
                handle_file_intent(
                    service,
                    &ctx,
                    &request,
                    response_mode,
                    &mut sections,
                    &mut next_actions,
                    &mut next_cursor,
                )
                .await
            }
            ReadPackIntent::Grep => {
                handle_grep_intent(
                    service,
                    &ctx,
                    &request,
                    response_mode,
                    &mut sections,
                    &mut next_actions,
                    &mut next_cursor,
                )
                .await
            }
            ReadPackIntent::Query => {
                handle_query_intent(
                    service,
                    &ctx,
                    &request,
                    response_mode,
                    QueryIntentPolicy { allow_secrets },
                    &mut sections,
                )
                .await
            }
            ReadPackIntent::Onboarding => {
                handle_onboarding_intent(&ctx, &request, response_mode, &facts, &mut sections).await
            }
            ReadPackIntent::Memory => {
                handle_memory_intent(
                    service,
                    &ctx,
                    &request,
                    response_mode,
                    &mut sections,
                    &mut next_actions,
                    &mut next_cursor,
                )
                .await
            }
            ReadPackIntent::Recall => {
                handle_recall_intent(
                    service,
                    &ctx,
                    &request,
                    response_mode,
                    semantic_index_fresh,
                    &mut sections,
                    &mut next_cursor,
                )
                .await
            }
        }
    };
    let handler_result =
        match tokio::time::timeout(Duration::from_millis(timeout_ms), handler_future).await {
            Ok(result) => result,
            Err(_) => {
                let mut result = ReadPackResult {
                    version: VERSION,
                    intent,
                    root: ctx.root_display.clone(),
                    sections,
                    next_actions,
                    next_cursor,
                    budget: ReadPackBudget {
                        max_chars: ctx.max_chars,
                        used_chars: 0,
                        truncated: true,
                        truncation: Some(ReadPackTruncation::Timeout),
                    },
                    meta: meta_for_output.clone(),
                };
                overlap_dedupe_snippet_sections(&mut result.sections);
                if response_mode != ResponseMode::Full {
                    strip_snippet_reasons_for_output(&mut result.sections, true);
                }
                apply_meta_to_sections(&mut result.sections);
                let mut result =
                    match finalize_and_trim(result, &ctx, &request, intent, response_mode) {
                        Ok(value) => value,
                        Err(result) => return Ok(attach_meta(result, meta.clone())),
                    };
                repair_cursor_after_trim(
                    service,
                    &ctx,
                    &request,
                    intent,
                    response_mode,
                    &mut result,
                )
                .await;
                let _ = finalize_read_pack_budget(&mut result);
                while result.budget.used_chars > ctx.max_chars && result.sections.len() > 1 {
                    result.sections.pop();
                    result.next_actions.clear();
                    result.next_cursor = None;
                    repair_cursor_after_trim(
                        service,
                        &ctx,
                        &request,
                        intent,
                        response_mode,
                        &mut result,
                    )
                    .await;
                    let _ = finalize_read_pack_budget(&mut result);
                    result.budget.truncated = true;
                    if result.budget.truncation.is_none() {
                        result.budget.truncation = Some(ReadPackTruncation::MaxChars);
                    }
                }
                result.budget.truncated = true;
                result.budget.truncation = Some(ReadPackTruncation::Timeout);

                if response_mode != ResponseMode::Full {
                    strip_snippet_reasons_for_output(&mut result.sections, false);
                    let _ = finalize_read_pack_budget(&mut result);
                }

                note_session_working_set_from_read_pack_result(service, &result).await;

                let mut doc = render_read_pack_context_doc(&result, response_mode);
                loop {
                    if doc.chars().count() <= ctx.max_chars {
                        let output = CallToolResult::success(vec![Content::text(doc)]);
                        return Ok(attach_structured_content(
                            output,
                            &result,
                            result.meta.clone().unwrap_or_default(),
                            "read_pack",
                        ));
                    }
                    let cur_chars = doc.chars().count();
                    if cur_chars <= 1 {
                        break;
                    }
                    doc = truncate_to_chars(&doc, cur_chars.div_ceil(2));
                }
                let output = CallToolResult::success(vec![Content::text(doc)]);
                return Ok(attach_structured_content(
                    output,
                    &result,
                    result.meta.clone().unwrap_or_default(),
                    "read_pack",
                ));
            }
        };
    if let Err(result) = handler_result {
        return Ok(attach_meta(result, meta.clone()));
    }

    overlap_dedupe_snippet_sections(&mut sections);
    if response_mode != ResponseMode::Full {
        strip_snippet_reasons_for_output(&mut sections, true);
    }
    apply_meta_to_sections(&mut sections);
    let result = ReadPackResult {
        version: VERSION,
        intent,
        root: ctx.root_display.clone(),
        sections,
        next_actions,
        next_cursor,
        budget: ReadPackBudget {
            max_chars: ctx.max_chars,
            used_chars: 0,
            truncated: false,
            truncation: None,
        },
        meta: meta_for_output,
    };

    let result = match finalize_and_trim(result, &ctx, &request, intent, response_mode) {
        Ok(value) => value,
        Err(result) => return Ok(attach_meta(result, meta.clone())),
    };
    let mut result = result;
    repair_cursor_after_trim(service, &ctx, &request, intent, response_mode, &mut result).await;
    let _ = finalize_read_pack_budget(&mut result);
    while result.budget.used_chars > ctx.max_chars && result.sections.len() > 1 {
        result.sections.pop();
        result.next_actions.clear();
        result.next_cursor = None;
        repair_cursor_after_trim(service, &ctx, &request, intent, response_mode, &mut result).await;
        let _ = finalize_read_pack_budget(&mut result);
        result.budget.truncated = true;
        if result.budget.truncation.is_none() {
            result.budget.truncation = Some(ReadPackTruncation::MaxChars);
        }
    }

    if response_mode != ResponseMode::Full {
        strip_snippet_reasons_for_output(&mut result.sections, false);
        let _ = finalize_read_pack_budget(&mut result);
    }

    note_session_working_set_from_read_pack_result(service, &result).await;

    // `.context` output is returned as plain text (no JSON envelope).
    //
    // We still apply a defensive shrink loop because pack assembly may occasionally overshoot
    // under tiny budgets.
    let mut doc = render_read_pack_context_doc(&result, response_mode);
    loop {
        if doc.chars().count() <= ctx.max_chars {
            let output = CallToolResult::success(vec![Content::text(doc)]);
            return Ok(attach_structured_content(
                output,
                &result,
                result.meta.clone().unwrap_or_default(),
                "read_pack",
            ));
        }
        let cur_chars = doc.chars().count();
        if cur_chars <= 1 {
            break;
        }
        doc = truncate_to_chars(&doc, cur_chars.div_ceil(2));
    }
    let output = CallToolResult::success(vec![Content::text(doc)]);
    Ok(attach_structured_content(
        output,
        &result,
        result.meta.clone().unwrap_or_default(),
        "read_pack",
    ))
}

#[cfg(test)]
mod tests {
    use super::super::router::cursor_alias::expand_cursor_alias;
    use super::super::{decode_cursor, ContextFinderService};
    use super::candidates::collect_github_workflow_candidates;
    use super::project_facts::PROJECT_FACTS_VERSION;
    use super::{
        build_context, collect_memory_file_candidates, finalize_and_trim, handle_recall_intent,
        is_disallowed_memory_file, parse_recall_question_directives, recall_question_policy,
        render_read_pack_context_doc, repair_recall_cursor_after_trim, ProjectFactsResult,
        ReadPackBudget, ReadPackIntent, ReadPackRecallCursorV1, ReadPackRecallResult,
        ReadPackRequest, ReadPackResult, ReadPackSection, ReadPackSnippet, ReadPackSnippetKind,
        ReadPackTruncation, RecallQuestionMode, ResponseMode,
    };
    use context_protocol::ToolNextAction;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn base_request() -> ReadPackRequest {
        ReadPackRequest {
            path: Some(".".to_string()),
            intent: None,
            file: None,
            pattern: None,
            query: None,
            ask: None,
            questions: None,
            topics: None,
            file_pattern: None,
            include_paths: None,
            exclude_paths: None,
            before: None,
            after: None,
            case_sensitive: None,
            start_line: None,
            max_lines: None,
            max_chars: None,
            response_mode: None,
            timeout_ms: None,
            cursor: None,
            prefer_code: None,
            include_docs: None,
            allow_secrets: None,
        }
    }

    #[test]
    fn render_read_pack_renders_context_pack_and_next_actions_in_text() {
        let pack = context_search::ContextPackOutput {
            version: 1,
            query: "find alpha entrypoint".to_string(),
            model_id: "stub".to_string(),
            profile: "quality".to_string(),
            items: vec![context_search::ContextPackItem {
                id: "i0".to_string(),
                role: "primary".to_string(),
                file: "src/lib.rs".to_string(),
                start_line: 1,
                end_line: 3,
                symbol: Some("alpha".to_string()),
                chunk_type: Some("code".to_string()),
                score: 0.9,
                imports: Vec::new(),
                content: "pub fn alpha() -> i32 { 1 }\n".to_string(),
                relationship: None,
                distance: None,
            }],
            budget: context_search::ContextPackBudget {
                max_chars: 2000,
                used_chars: 200,
                truncated: false,
                dropped_items: 0,
                truncation: None,
            },
            next_actions: vec![ToolNextAction {
                tool: "cat".to_string(),
                args: serde_json::json!({ "file": "src/lib.rs", "start_line": 1, "max_lines": 40 }),
                reason: "Open the referenced file for more context.".to_string(),
            }],
            meta: context_indexer::ToolMeta::default(),
        };

        let result = ReadPackResult {
            version: 1,
            intent: ReadPackIntent::Query,
            root: ".".to_string(),
            sections: vec![ReadPackSection::ContextPack {
                result: serde_json::to_value(&pack).expect("pack should serialize"),
            }],
            next_actions: vec![ToolNextAction {
                tool: "read_pack".to_string(),
                args: serde_json::json!({ "intent": "query", "query": "alpha", "max_chars": 4000 }),
                reason: "Retry with a larger budget.".to_string(),
            }],
            next_cursor: None,
            budget: ReadPackBudget {
                max_chars: 2000,
                used_chars: 200,
                truncated: false,
                truncation: None,
            },
            meta: None,
        };

        let text = render_read_pack_context_doc(&result, ResponseMode::Full);
        assert!(
            text.contains("context_pack:"),
            "expected context_pack summary, got:\n{text}"
        );
        assert!(
            text.contains("R: src/lib.rs:1"),
            "expected item file ref, got:\n{text}"
        );
        assert!(
            text.contains("next_actions:"),
            "expected next_actions section, got:\n{text}"
        );
        assert!(
            !text.contains("structured_content"),
            "must not mention structured_content in text output:\n{text}"
        );
    }

    #[test]
    fn build_context_reserves_headroom() {
        let mut request = base_request();
        request.max_chars = Some(20_000);

        let ctx = build_context(&request, PathBuf::from("."), ".".to_string())
            .unwrap_or_else(|_| panic!("build_context should succeed"));
        assert_eq!(ctx.inner_max_chars, 19_200);
    }

    #[test]
    fn build_context_never_exceeds_max_chars() {
        let mut request = base_request();
        request.max_chars = Some(500);

        let ctx = build_context(&request, PathBuf::from("."), ".".to_string())
            .unwrap_or_else(|_| panic!("build_context should succeed"));
        assert_eq!(ctx.max_chars, 500);
        assert_eq!(ctx.inner_max_chars, 436);
    }

    #[test]
    fn memory_candidates_block_secrets_allow_templates() {
        assert!(is_disallowed_memory_file(".env"));
        assert!(is_disallowed_memory_file(".env.local"));
        assert!(is_disallowed_memory_file("prod.env"));
        assert!(is_disallowed_memory_file("id_rsa"));
        assert!(is_disallowed_memory_file("secrets/id_ed25519"));
        assert!(is_disallowed_memory_file("cert.pem"));
        assert!(is_disallowed_memory_file("keys/token.pfx"));

        assert!(!is_disallowed_memory_file(".env.example"));
        assert!(!is_disallowed_memory_file(".env.sample"));
        assert!(!is_disallowed_memory_file(".env.template"));
        assert!(!is_disallowed_memory_file(".env.dist"));
    }

    #[test]
    fn github_workflow_candidates_are_sorted_and_bounded() {
        let temp = tempdir().unwrap();
        let workflows_dir = temp.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();

        std::fs::write(workflows_dir.join("b.yml"), b"name: b\n").unwrap();
        std::fs::write(workflows_dir.join("a.yaml"), b"name: a\n").unwrap();
        std::fs::write(workflows_dir.join("c.txt"), b"ignore\n").unwrap();

        let mut seen = std::collections::HashSet::new();
        let candidates = collect_github_workflow_candidates(temp.path(), &mut seen);

        assert_eq!(
            candidates,
            vec![".github/workflows/a.yaml", ".github/workflows/b.yml"]
        );
    }

    #[test]
    fn memory_candidates_fallback_discovers_doc_like_files() {
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("HACKING.md"), b"how to hack\n").unwrap();

        let candidates = collect_memory_file_candidates(temp.path());
        assert!(
            candidates.iter().any(|c| c == "HACKING.md"),
            "expected fallback doc discovery to include HACKING.md"
        );
    }

    #[test]
    fn overlap_dedupe_removes_contained_snippet_spans() {
        let mut sections = vec![
            ReadPackSection::ProjectFacts {
                result: ProjectFactsResult {
                    version: 1,
                    ecosystems: Vec::new(),
                    build_tools: Vec::new(),
                    ci: Vec::new(),
                    contracts: Vec::new(),
                    key_dirs: Vec::new(),
                    modules: Vec::new(),
                    entry_points: Vec::new(),
                    key_configs: Vec::new(),
                },
            },
            ReadPackSection::Snippet {
                result: ReadPackSnippet {
                    file: "src/lib.rs".to_string(),
                    start_line: 1,
                    end_line: 80,
                    content: "fn a() {}\n".to_string(),
                    kind: Some(ReadPackSnippetKind::Code),
                    reason: Some(super::REASON_NEEDLE_FILE_SLICE.to_string()),
                    next_cursor: None,
                },
            },
            ReadPackSection::Snippet {
                result: ReadPackSnippet {
                    file: "src/lib.rs".to_string(),
                    start_line: 10,
                    end_line: 30,
                    content: "fn b() {}\n".to_string(),
                    kind: Some(ReadPackSnippetKind::Code),
                    reason: Some(super::REASON_NEEDLE_FILE_SLICE.to_string()),
                    next_cursor: None,
                },
            },
        ];

        super::overlap_dedupe_snippet_sections(&mut sections);
        let snippet_count = sections
            .iter()
            .filter(|section| matches!(section, ReadPackSection::Snippet { .. }))
            .count();
        assert_eq!(snippet_count, 1, "expected contained snippet to be deduped");
    }

    #[test]
    fn strip_reasons_keeps_focus_file_only_when_requested() {
        let mut sections = vec![
            ReadPackSection::Snippet {
                result: ReadPackSnippet {
                    file: "src/main.rs".to_string(),
                    start_line: 1,
                    end_line: 10,
                    content: "fn main() {}\n".to_string(),
                    kind: Some(ReadPackSnippetKind::Code),
                    reason: Some(super::REASON_ANCHOR_FOCUS_FILE.to_string()),
                    next_cursor: None,
                },
            },
            ReadPackSection::Snippet {
                result: ReadPackSnippet {
                    file: "README.md".to_string(),
                    start_line: 1,
                    end_line: 5,
                    content: "Read me\n".to_string(),
                    kind: Some(ReadPackSnippetKind::Doc),
                    reason: Some(super::REASON_ANCHOR_DOC.to_string()),
                    next_cursor: None,
                },
            },
        ];

        super::strip_snippet_reasons_for_output(&mut sections, true);
        let focus_reason = match &sections[0] {
            ReadPackSection::Snippet { result } => result.reason.clone(),
            _ => None,
        };
        let other_reason = match &sections[1] {
            ReadPackSection::Snippet { result } => result.reason.clone(),
            _ => None,
        };
        assert_eq!(
            focus_reason.as_deref(),
            Some(super::REASON_ANCHOR_FOCUS_FILE),
            "expected focus-file reason to remain when keep_focus_file=true"
        );
        assert!(
            other_reason.is_none(),
            "expected non-focus reasons to be stripped"
        );

        super::strip_snippet_reasons_for_output(&mut sections, false);
        let focus_reason = match &sections[0] {
            ReadPackSection::Snippet { result } => result.reason.clone(),
            _ => None,
        };
        assert!(
            focus_reason.is_none(),
            "expected focus-file reason to be stripped when keep_focus_file=false"
        );
    }

    #[tokio::test]
    async fn memory_pack_prefers_unseen_docs_across_calls() {
        let service = ContextFinderService::new();

        let temp = tempdir().unwrap();
        let root = temp.path();

        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
        std::fs::create_dir_all(root.join(".vscode")).unwrap();

        std::fs::write(root.join("AGENTS.md"), b"agents\n").unwrap();
        std::fs::write(root.join("README.md"), b"readme\n").unwrap();
        std::fs::write(root.join("docs/README.md"), b"docs readme\n").unwrap();
        std::fs::write(root.join("docs/QUICK_START.md"), b"quick start\n").unwrap();
        std::fs::write(root.join("PHILOSOPHY.md"), b"philosophy\n").unwrap();
        std::fs::write(root.join("DEVELOPMENT.md"), b"dev\n").unwrap();
        std::fs::write(root.join("Cargo.toml"), b"[package]\nname = \"x\"\n").unwrap();
        std::fs::write(
            root.join(".github/workflows/ci.yml"),
            b"name: CI\non: [push]\n",
        )
        .unwrap();
        std::fs::write(root.join(".vscode/settings.json"), b"{\"x\":1}\n").unwrap();

        let root_display = root.to_string_lossy().to_string();
        let mut request = base_request();
        request.path = Some(root_display.clone());
        request.max_chars = Some(8_000);
        request.response_mode = Some(ResponseMode::Facts);

        let ctx = build_context(&request, root.to_path_buf(), root_display.clone()).unwrap();

        let mut sections1 = vec![ReadPackSection::ProjectFacts {
            result: ProjectFactsResult {
                version: 1,
                ecosystems: Vec::new(),
                build_tools: Vec::new(),
                ci: Vec::new(),
                contracts: Vec::new(),
                key_dirs: Vec::new(),
                modules: Vec::new(),
                entry_points: Vec::new(),
                key_configs: Vec::new(),
            },
        }];
        let mut next_actions = Vec::new();
        let mut next_cursor = None;
        super::handle_memory_intent(
            &service,
            &ctx,
            &request,
            ResponseMode::Facts,
            &mut sections1,
            &mut next_actions,
            &mut next_cursor,
        )
        .await
        .unwrap();

        let files1: Vec<String> = sections1
            .iter()
            .filter_map(|section| match section {
                ReadPackSection::Snippet { result } => Some(result.file.clone()),
                ReadPackSection::FileSlice { result } => Some(result.file.clone()),
                _ => None,
            })
            .collect();
        assert!(
            files1.iter().any(|f| f == "AGENTS.md"),
            "expected AGENTS.md in first memory pack"
        );
        assert!(
            files1.iter().any(|f| f == "README.md"),
            "expected README.md in first memory pack"
        );

        {
            let mut session = service.session.lock().await;
            for file in &files1 {
                session.note_seen_snippet_file(file);
            }
        }

        let mut sections2 = vec![ReadPackSection::ProjectFacts {
            result: ProjectFactsResult {
                version: 1,
                ecosystems: Vec::new(),
                build_tools: Vec::new(),
                ci: Vec::new(),
                contracts: Vec::new(),
                key_dirs: Vec::new(),
                modules: Vec::new(),
                entry_points: Vec::new(),
                key_configs: Vec::new(),
            },
        }];
        let mut next_actions = Vec::new();
        let mut next_cursor = None;
        super::handle_memory_intent(
            &service,
            &ctx,
            &request,
            ResponseMode::Facts,
            &mut sections2,
            &mut next_actions,
            &mut next_cursor,
        )
        .await
        .unwrap();

        let files2: Vec<String> = sections2
            .iter()
            .filter_map(|section| match section {
                ReadPackSection::Snippet { result } => Some(result.file.clone()),
                ReadPackSection::FileSlice { result } => Some(result.file.clone()),
                _ => None,
            })
            .collect();
        assert!(
            files2.iter().any(|f| f == "AGENTS.md"),
            "expected AGENTS.md in second memory pack (anchor)"
        );
        assert!(
            files2.iter().any(|f| f == "README.md"),
            "expected README.md in second memory pack (anchor)"
        );

        let non_anchor1: std::collections::HashSet<String> = files1
            .into_iter()
            .filter(|f| f != "AGENTS.md" && f != "README.md")
            .collect();
        let non_anchor2: std::collections::HashSet<String> = files2
            .into_iter()
            .filter(|f| f != "AGENTS.md" && f != "README.md")
            .collect();
        assert!(
            non_anchor2.difference(&non_anchor1).next().is_some(),
            "expected second memory pack to include at least one new non-anchor file"
        );
    }

    #[test]
    fn recall_question_directives_support_fast_deep_and_scoping() {
        let temp = tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src").join("main.rs"), b"fn main() {}\n").unwrap();

        let (cleaned, directives) =
            parse_recall_question_directives("deep k:5 ctx:4 in:src lit: fn main()", temp.path());
        assert_eq!(directives.mode, RecallQuestionMode::Deep);
        assert_eq!(directives.snippet_limit, Some(5));
        assert_eq!(directives.grep_context, Some(4));
        assert_eq!(directives.include_paths, vec!["src".to_string()]);
        assert_eq!(cleaned, "lit: fn main()".to_string());

        let (cleaned, directives) =
            parse_recall_question_directives("fast not:src lit: cargo test", temp.path());
        assert_eq!(directives.mode, RecallQuestionMode::Fast);
        assert_eq!(directives.exclude_paths, vec!["src".to_string()]);
        assert_eq!(cleaned, "lit: cargo test".to_string());

        let (_cleaned, directives) =
            parse_recall_question_directives("index:5s lit: cursor", temp.path());
        assert_eq!(directives.mode, RecallQuestionMode::Deep);
    }

    #[test]
    fn recall_policy_respects_fast_deep_and_freshness() {
        let policy = recall_question_policy(RecallQuestionMode::Fast, false);
        assert!(!policy.allow_semantic);

        let policy = recall_question_policy(RecallQuestionMode::Auto, false);
        assert!(!policy.allow_semantic);

        let policy = recall_question_policy(RecallQuestionMode::Auto, true);
        assert!(policy.allow_semantic);

        let policy = recall_question_policy(RecallQuestionMode::Deep, false);
        assert!(policy.allow_semantic);
    }

    #[test]
    fn auto_intent_routes_onboarding_for_onboarding_like_query() {
        let mut request = base_request();
        request.query = Some("how to run tests".to_string());
        request.intent = None;

        let intent = super::resolve_intent(&request).unwrap();
        assert_eq!(intent, ReadPackIntent::Onboarding);
    }

    #[tokio::test]
    async fn onboarding_intent_in_facts_mode_emits_snippets() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("README.md"), b"## Quick start\nrun tests\n").unwrap();
        std::fs::write(root.join("AGENTS.md"), b"agents\n").unwrap();

        let mut request = base_request();
        request.path = Some(root.to_string_lossy().to_string());
        request.max_chars = Some(4_000);

        let ctx = build_context(&request, root.to_path_buf(), request.path.clone().unwrap())
            .unwrap_or_else(|_| panic!("build_context should succeed"));

        let mut sections = Vec::new();
        let facts = ProjectFactsResult {
            version: PROJECT_FACTS_VERSION,
            ecosystems: vec!["rust".to_string()],
            build_tools: vec!["cargo".to_string()],
            ci: Vec::new(),
            contracts: Vec::new(),
            key_dirs: Vec::new(),
            modules: Vec::new(),
            entry_points: Vec::new(),
            key_configs: Vec::new(),
        };
        super::handle_onboarding_intent(&ctx, &request, ResponseMode::Facts, &facts, &mut sections)
            .await
            .unwrap();

        assert!(
            sections
                .iter()
                .any(|s| matches!(s, ReadPackSection::Snippet { .. })),
            "expected onboarding to emit snippet sections in facts mode"
        );
        assert!(
            !sections
                .iter()
                .any(|s| matches!(s, ReadPackSection::RepoOnboardingPack { .. })),
            "expected onboarding not to emit full repo_onboarding_pack section in facts mode"
        );
    }

    #[tokio::test]
    async fn onboarding_facts_tight_budget_still_emits_anchor_doc_snippet() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("AGENTS.md"), b"# AGENTS\n\nline\nline\nline\n").unwrap();

        let mut request = base_request();
        request.path = Some(root.to_string_lossy().to_string());
        request.max_chars = Some(1_200);

        let ctx = build_context(&request, root.to_path_buf(), request.path.clone().unwrap())
            .unwrap_or_else(|_| panic!("build_context should succeed"));

        let mut sections = Vec::new();
        let facts = ProjectFactsResult {
            version: PROJECT_FACTS_VERSION,
            ecosystems: vec!["rust".to_string()],
            build_tools: vec!["cargo".to_string()],
            ci: Vec::new(),
            contracts: Vec::new(),
            key_dirs: Vec::new(),
            modules: Vec::new(),
            entry_points: Vec::new(),
            key_configs: Vec::new(),
        };
        super::handle_onboarding_intent(&ctx, &request, ResponseMode::Facts, &facts, &mut sections)
            .await
            .unwrap();

        assert!(
            sections
                .iter()
                .any(|s| matches!(s, ReadPackSection::Snippet { .. })),
            "expected onboarding facts to emit at least one snippet under a tight budget"
        );
    }

    #[tokio::test]
    async fn onboarding_tests_question_emits_command_snippet_via_grep() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        std::fs::write(
            root.join("AGENTS.md"),
            b"# Agent rules\n\n...\n\nQuality gates:\nCONTEXT_FINDER_EMBEDDING_MODE=stub cargo test --workspace\n",
        )
        .unwrap();

        let mut request = base_request();
        request.path = Some(root.to_string_lossy().to_string());
        request.ask = Some("how to run tests".to_string());
        request.max_chars = Some(1_800);

        let ctx = build_context(&request, root.to_path_buf(), request.path.clone().unwrap())
            .unwrap_or_else(|_| panic!("build_context should succeed"));

        let mut sections = Vec::new();
        let facts = ProjectFactsResult {
            version: PROJECT_FACTS_VERSION,
            ecosystems: vec!["rust".to_string()],
            build_tools: vec!["cargo".to_string()],
            ci: Vec::new(),
            contracts: Vec::new(),
            key_dirs: Vec::new(),
            modules: Vec::new(),
            entry_points: Vec::new(),
            key_configs: Vec::new(),
        };
        super::handle_onboarding_intent(&ctx, &request, ResponseMode::Facts, &facts, &mut sections)
            .await
            .unwrap();

        let found = sections.iter().any(|section| match section {
            ReadPackSection::Snippet { result } => result.content.contains("cargo test"),
            _ => false,
        });
        assert!(
            found,
            "expected onboarding to surface a test command via grep snippet"
        );
    }

    #[tokio::test]
    async fn recall_upgrades_doc_only_matches_to_code_when_possible() {
        let service = ContextFinderService::new();

        let temp = tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("README.md"), b"velocity docs mention\n").unwrap();
        std::fs::write(root.join("src").join("main.rs"), b"fn velocity() {}\n").unwrap();

        let root_display = root.to_string_lossy().to_string();
        let mut request = base_request();
        request.path = Some(root_display.clone());
        request.questions = Some(vec!["where is velocity computed".to_string()]);
        // Tight budget so snippet_limit is likely 1 and naive grep would return README.md first.
        request.max_chars = Some(1_200);
        request.response_mode = Some(ResponseMode::Facts);

        let ctx = build_context(&request, root.to_path_buf(), root_display.clone()).unwrap();

        // Sanity check: grep fallback must find at least one match in the repo under tight budgets.
        let keyword = super::best_keyword_pattern("where is velocity computed")
            .expect("expected keyword extraction to succeed");
        let (direct_snippets, _) = super::snippets_from_grep_filtered(
            &ctx,
            &keyword,
            super::GrepSnippetParams {
                file: None,
                file_pattern: None,
                before: 12,
                after: 12,
                max_hunks: 1,
                max_chars: 900,
                case_sensitive: false,
                allow_secrets: false,
            },
            &[],
            &[],
            None,
        )
        .await
        .unwrap();
        assert!(
            !direct_snippets.is_empty(),
            "expected direct grep fallback to find velocity"
        );
        let mut sections = Vec::new();
        let mut next_cursor = None;

        handle_recall_intent(
            &service,
            &ctx,
            &request,
            ResponseMode::Facts,
            false,
            &mut sections,
            &mut next_cursor,
        )
        .await
        .unwrap();

        let recall = sections.iter().find_map(|section| match section {
            ReadPackSection::Recall { result } => Some(result),
            _ => None,
        });
        let recall = recall.expect("expected recall section");
        assert_eq!(
            recall.snippets.len(),
            1,
            "expected a single snippet under budget"
        );
        assert_eq!(
            recall.snippets[0].file, "src/main.rs",
            "expected recall to prefer code over README matches"
        );
    }

    #[test]
    fn cursor_pagination_marks_budget_truncated_even_under_max_chars() {
        let mut request = base_request();
        request.max_chars = Some(2_000);
        let ctx = build_context(&request, PathBuf::from("."), ".".to_string())
            .unwrap_or_else(|_| panic!("build_context should succeed"));

        let result = ReadPackResult {
            version: 1,
            intent: ReadPackIntent::Memory,
            root: ".".to_string(),
            sections: vec![ReadPackSection::ProjectFacts {
                result: ProjectFactsResult {
                    version: 1,
                    ecosystems: Vec::new(),
                    build_tools: Vec::new(),
                    ci: Vec::new(),
                    contracts: Vec::new(),
                    key_dirs: Vec::new(),
                    modules: Vec::new(),
                    entry_points: Vec::new(),
                    key_configs: Vec::new(),
                },
            }],
            next_actions: Vec::new(),
            next_cursor: Some("cfcs1:AAAAAAAAAA".to_string()),
            budget: ReadPackBudget {
                max_chars: ctx.max_chars,
                used_chars: 0,
                truncated: false,
                truncation: None,
            },
            meta: None,
        };

        let result = finalize_and_trim(
            result,
            &ctx,
            &request,
            ReadPackIntent::Memory,
            ResponseMode::Facts,
        )
        .unwrap_or_else(|_| panic!("finalize_and_trim should succeed"));

        assert!(result.budget.truncated);
        assert_eq!(result.budget.truncation, Some(ReadPackTruncation::MaxItems));
    }

    #[tokio::test]
    async fn recall_cursor_repair_overwrites_existing_cursor() {
        let service = ContextFinderService::new();

        let temp = tempdir().unwrap();
        let root_display = temp.path().to_string_lossy().to_string();

        let mut request = base_request();
        request.path = Some(root_display.clone());
        request.max_chars = Some(6_000);
        request.questions = Some(vec![
            "Q1: identity".to_string(),
            "Q2: entrypoints".to_string(),
            "Q3: commands".to_string(),
        ]);

        let ctx = build_context(&request, temp.path().to_path_buf(), root_display.clone()).unwrap();

        let mut result = ReadPackResult {
            version: 1,
            intent: ReadPackIntent::Recall,
            root: root_display.clone(),
            sections: vec![
                ReadPackSection::ProjectFacts {
                    result: ProjectFactsResult {
                        version: 1,
                        ecosystems: Vec::new(),
                        build_tools: Vec::new(),
                        ci: Vec::new(),
                        contracts: Vec::new(),
                        key_dirs: Vec::new(),
                        modules: Vec::new(),
                        entry_points: Vec::new(),
                        key_configs: Vec::new(),
                    },
                },
                ReadPackSection::Recall {
                    result: ReadPackRecallResult {
                        question: "Q1: identity".to_string(),
                        snippets: Vec::new(),
                    },
                },
            ],
            next_actions: Vec::new(),
            // Simulate the "buggy" state: a cursor already exists, but would skip Q2 under trim.
            next_cursor: Some("cfcs1:AAAAAAAAAA".to_string()),
            budget: ReadPackBudget {
                max_chars: ctx.max_chars,
                used_chars: 0,
                truncated: true,
                truncation: Some(ReadPackTruncation::MaxChars),
            },
            meta: None,
        };

        repair_recall_cursor_after_trim(&service, &ctx, &request, ResponseMode::Facts, &mut result)
            .await;

        let cursor = result.next_cursor.as_deref().expect("expected next_cursor");
        let expanded = expand_cursor_alias(&service, cursor)
            .await
            .expect("cursor alias should expand in tests");
        let decoded: ReadPackRecallCursorV1 =
            decode_cursor(&expanded).expect("cursor should decode");
        assert_eq!(
            decoded.questions,
            vec!["Q2: entrypoints".to_string(), "Q3: commands".to_string()]
        );
    }

    #[test]
    fn finalize_and_trim_recall_prefers_dropping_snippets_over_questions() {
        let mut request = base_request();
        request.max_chars = Some(3_000);
        let ctx = build_context(&request, PathBuf::from("."), ".".to_string()).unwrap();

        let big = "x".repeat(1_600);
        let result = ReadPackResult {
            version: 1,
            intent: ReadPackIntent::Recall,
            root: ".".to_string(),
            sections: vec![
                ReadPackSection::ProjectFacts {
                    result: ProjectFactsResult {
                        version: 1,
                        ecosystems: Vec::new(),
                        build_tools: Vec::new(),
                        ci: Vec::new(),
                        contracts: Vec::new(),
                        key_dirs: Vec::new(),
                        modules: Vec::new(),
                        entry_points: Vec::new(),
                        key_configs: Vec::new(),
                    },
                },
                ReadPackSection::Recall {
                    result: ReadPackRecallResult {
                        question: "Q1".to_string(),
                        snippets: vec![
                            ReadPackSnippet {
                                file: "README.md".to_string(),
                                start_line: 1,
                                end_line: 10,
                                content: big.clone(),
                                kind: None,
                                reason: None,
                                next_cursor: None,
                            },
                            ReadPackSnippet {
                                file: "DEVELOPMENT.md".to_string(),
                                start_line: 1,
                                end_line: 10,
                                content: big.clone(),
                                kind: None,
                                reason: None,
                                next_cursor: None,
                            },
                            ReadPackSnippet {
                                file: "Cargo.toml".to_string(),
                                start_line: 1,
                                end_line: 10,
                                content: big,
                                kind: None,
                                reason: None,
                                next_cursor: None,
                            },
                        ],
                    },
                },
            ],
            next_actions: Vec::new(),
            next_cursor: None,
            budget: ReadPackBudget {
                max_chars: ctx.max_chars,
                used_chars: 0,
                truncated: false,
                truncation: None,
            },
            meta: None,
        };

        let trimmed = finalize_and_trim(
            result,
            &ctx,
            &request,
            ReadPackIntent::Recall,
            ResponseMode::Facts,
        )
        .unwrap();

        let recall = trimmed
            .sections
            .iter()
            .find_map(|section| match section {
                ReadPackSection::Recall { result } => Some(result),
                _ => None,
            })
            .expect("expected recall section to survive trimming");

        assert!(
            recall.snippets.len() < 3,
            "expected recall trimming to drop snippets before dropping the question"
        );
        assert!(
            !recall.snippets.is_empty(),
            "expected at least one snippet to remain for the question"
        );
    }
}
