use super::router::cursor_alias::{compact_cursor_alias, expand_cursor_alias};
use super::router::error::{
    attach_meta, attach_structured_content, invalid_cursor_with_meta_details, invalid_request_with,
    invalid_request_with_meta, meta_for_request, tool_error,
};
use super::{
    decode_cursor, encode_cursor, finalize_read_pack_budget, AutoIndexPolicy, CallToolResult,
    Content, ContextFinderService, McpError, ProjectFactsResult, ReadPackBudget, ReadPackIntent,
    ReadPackNextAction, ReadPackRecallResult, ReadPackRequest, ReadPackResult, ReadPackSection,
    ReadPackSnippet, ReadPackSnippetKind, ReadPackTruncation, ResponseMode, CURSOR_VERSION,
};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::cursor::cursor_fingerprint;
use context_indexer::{root_fingerprint, ToolMeta};
use context_protocol::ToolNextAction;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

mod context;
use context::{build_context, ReadPackContext};

mod anchor_scan;
mod candidates;
mod cursors;
mod fs_scan;
mod intent_file;
mod intent_grep;
mod intent_memory;
mod intent_onboarding;
mod intent_query;
mod intent_recall;
mod memory_cursor;
mod memory_overview;
mod memory_snippets;
mod project_facts;
mod recall;

use candidates::{collect_memory_file_candidates, is_disallowed_memory_file};
use cursors::{trim_chars, trimmed_non_empty_str, CursorHeader, ReadPackMemoryCursorV1};
use intent_file::handle_file_intent;
use intent_grep::handle_grep_intent;
use intent_memory::handle_memory_intent;
use intent_onboarding::handle_onboarding_intent;
use intent_query::{handle_query_intent, QueryIntentPolicy};
use intent_recall::{
    handle_recall_intent, repair_recall_cursor_after_trim, trim_recall_sections_for_budget,
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

fn call_error(code: &'static str, message: impl Into<String>) -> CallToolResult {
    tool_error(code, message)
}

fn truncate_vec<T>(values: &mut Vec<T>, max: usize) {
    if values.len() > max {
        values.truncate(max);
    }
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
                                let message = "Invalid cursor: cursor refers to a different project root than the current session; call `root_set` to switch projects (or pass `path`)."
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

    // DX convenience: callers often pass `path` as a *subdirectory or file within the project*
    // (e.g. `{ \"path\": \"src\", \"pattern\": \"foo\" }` or `{ \"path\": \"README.md\" }`).
    // In Context, `path` sets the project root.
    //
    // When the session already has a root, treat a relative `path` with no `file`/`file_pattern`
    // and no cursor as a file/file_pattern hint instead of switching the session root.
    let cursor_missing = trimmed_non_empty_str(request.cursor.as_deref()).is_none();
    let file_missing = trimmed_non_empty_str(request.file.as_deref()).is_none();
    let file_pattern_missing = trimmed_non_empty_str(request.file_pattern.as_deref()).is_none();
    if cursor_missing && file_missing && file_pattern_missing {
        if let Some(raw_path) = trimmed_non_empty_str(request.path.as_deref()) {
            let session_root = { service.session.lock().await.clone_root().map(|(r, _)| r) };
            if let Some(session_root) = session_root.as_ref() {
                let raw = Path::new(raw_path);
                if raw.is_absolute() {
                    if let Ok(canonical) = raw.canonicalize() {
                        if canonical.starts_with(session_root) {
                            if let Ok(rel) = canonical.strip_prefix(session_root) {
                                if let Some(rel) =
                                    crate::tools::dispatch::root::rel_path_string(rel)
                                {
                                    let is_file = std::fs::metadata(&canonical)
                                        .ok()
                                        .map(|meta| meta.is_file())
                                        .unwrap_or(false);
                                    if is_file {
                                        request.file = Some(rel);
                                    } else {
                                        let mut pattern = rel;
                                        if !pattern.ends_with('/') {
                                            pattern.push('/');
                                        }
                                        request.file_pattern = Some(pattern);
                                    }
                                    request.path = None;
                                }
                            }
                        }
                    }
                } else {
                    let normalized = raw_path.trim_start_matches("./");
                    if normalized == "." || normalized.is_empty() {
                        request.path = None;
                    } else {
                        let candidate = session_root.join(normalized);
                        let meta = std::fs::metadata(&candidate).ok();
                        let is_file = meta.as_ref().map(|m| m.is_file()).unwrap_or(false);
                        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                        if is_file {
                            request.file = Some(normalized.to_string());
                        } else {
                            let mut pattern = normalized.to_string();
                            if is_dir && !pattern.ends_with('/') {
                                pattern.push('/');
                            }
                            request.file_pattern = Some(pattern);
                        }
                        request.path = None;
                    }
                }
            }
        }
    }

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
mod tests;
