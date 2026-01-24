use super::super::router::context_pack::context_pack;
use super::super::router::cursor_alias::compact_cursor_alias;
use super::super::{
    compute_file_slice_result, compute_grep_context_result, decode_cursor, encode_cursor,
    ContextPackRequest, FileSliceRequest, GrepContextComputeOptions, GrepContextRequest,
};
use super::anchor_scan::best_anchor_line_for_kind;
use super::candidates::{
    collect_ops_file_candidates, config_candidate_score, is_disallowed_memory_file,
    ops_candidate_score,
};
use super::cursors::{
    normalize_optional_pattern, normalize_path_prefix_list, normalize_questions, normalize_topics,
    snippet_kind_for_path, trim_chars, trim_utf8_bytes, trimmed_non_empty_str,
    ReadPackRecallCursorStoredV1, ReadPackRecallCursorV1, DEFAULT_RECALL_SNIPPETS_PER_QUESTION,
    MAX_RECALL_FILTER_PATHS, MAX_RECALL_FILTER_PATH_BYTES, MAX_RECALL_SNIPPETS_PER_QUESTION,
};
use super::project_facts::compute_project_facts;
use super::recall::{
    extract_existing_file_ref, parse_path_token, recall_doc_candidate_score,
    recall_structural_intent, OpsIntent, RecallStructuralIntent,
};
use super::{
    call_error, entrypoint_candidate_score, finalize_read_pack_budget,
    invalid_cursor_with_meta_details, ContextFinderService, ProjectFactsResult, ReadPackContext,
    ReadPackRecallResult, ReadPackRequest, ReadPackResult, ReadPackSection, ReadPackSnippet,
    ReadPackSnippetKind, ResponseMode, ToolResult, CURSOR_VERSION, MAX_GREP_MATCHES,
    MAX_RECALL_INLINE_CURSOR_CHARS, REASON_HALO_CONTEXT_PACK_PRIMARY, REASON_NEEDLE_FILE_SLICE,
    REASON_NEEDLE_GREP_HUNK,
};
use crate::tools::cursor::cursor_fingerprint;
use crate::tools::schemas::content_format::ContentFormat;
use context_indexer::{root_fingerprint, ToolMeta};
use context_search::QueryClassifier;
use regex::RegexBuilder;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;

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

pub(super) fn trim_recall_sections_for_budget(
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

pub(super) async fn repair_recall_cursor_after_trim(
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

pub(super) fn best_keyword_pattern(question: &str) -> Option<String> {
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

pub(super) fn recall_question_tokens(question: &str) -> Vec<String> {
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
        ) {
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
        let (found, _cursor) = snippets_from_grep_filtered(
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

        if found.is_empty() {
            continue;
        }

        if idx == 0 {
            found_code = found;
            break;
        }

        // Second chance: narrow to known code roots to avoid README-first matches.
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

pub(super) struct GrepSnippetParams {
    pub(super) file: Option<String>,
    pub(super) file_pattern: Option<String>,
    pub(super) before: usize,
    pub(super) after: usize,
    pub(super) max_hunks: usize,
    pub(super) max_chars: usize,
    pub(super) case_sensitive: bool,
    pub(super) allow_secrets: bool,
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
            content_max_chars: super::super::router::grep_context::grep_context_content_budget(
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

pub(super) async fn snippets_from_grep_filtered(
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
    let mut seen = HashSet::new();

    for prefix in include_paths.iter().take(MAX_RECALL_FILTER_PATHS) {
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
            end_line: None,
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
pub(super) enum RecallQuestionMode {
    #[default]
    Auto,
    Fast,
    Deep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RecallQuestionPolicy {
    pub(super) allow_semantic: bool,
}

pub(super) fn recall_question_policy(
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
pub(super) struct RecallQuestionDirectives {
    pub(super) mode: RecallQuestionMode,
    pub(super) snippet_limit: Option<usize>,
    pub(super) grep_context: Option<usize>,
    pub(super) include_paths: Vec<String>,
    pub(super) exclude_paths: Vec<String>,
    pub(super) file_pattern: Option<String>,
    pub(super) file_ref: Option<(String, Option<usize>)>,
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

pub(super) fn parse_recall_question_directives(
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

pub(super) async fn handle_recall_intent(
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
                let tool_result = context_pack(
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
