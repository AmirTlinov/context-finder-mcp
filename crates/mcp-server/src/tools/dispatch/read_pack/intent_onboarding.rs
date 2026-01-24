use super::super::router::grep_context::grep_context_content_budget;
use super::super::{
    compute_grep_context_result, compute_repo_onboarding_pack_result, GrepContextComputeOptions,
    GrepContextRequest, RepoOnboardingPackRequest,
};
use super::candidates::collect_ops_file_candidates;
use super::cursors::{snippet_kind_for_path, trimmed_non_empty_str};
use super::{
    call_error, ReadPackContext, ReadPackRequest, ReadPackSection, ReadPackSnippet, ResponseMode,
    REASON_ANCHOR_DOC, REASON_NEEDLE_GREP_HUNK,
};
use crate::tools::file_slice::compute_onboarding_doc_slice;
use crate::tools::schemas::content_format::ContentFormat;
use regex::RegexBuilder;
use std::collections::HashSet;

pub(super) async fn handle_onboarding_intent(
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    facts: &super::ProjectFactsResult,
    sections: &mut Vec<ReadPackSection>,
) -> super::ToolResult<()> {
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
        let prompt = prompt.to_ascii_lowercase();
        let has_tests = prompt.contains("test") || prompt.contains("тест");
        let has_run = prompt.contains("run") || prompt.contains("запуск");
        let has_build = prompt.contains("build") || prompt.contains("сбор");
        let has_install = prompt.contains("install") || prompt.contains("установ");
        let has_ci = prompt.contains("ci") || prompt.contains("pipeline");
        let has_structure = prompt.contains("structure")
            || prompt.contains("architecture")
            || prompt.contains("структур")
            || prompt.contains("архитект");

        if has_tests {
            OnboardingTopic::Tests
        } else if has_run {
            OnboardingTopic::Run
        } else if has_build {
            OnboardingTopic::Build
        } else if has_install {
            OnboardingTopic::Install
        } else if has_ci {
            OnboardingTopic::CI
        } else if has_structure {
            OnboardingTopic::Structure
        } else {
            OnboardingTopic::Unknown
        }
    }

    fn command_grep_pattern(
        topic: OnboardingTopic,
        facts: &super::ProjectFactsResult,
    ) -> Option<String> {
        let ecosystems = facts
            .ecosystems
            .iter()
            .map(|item| item.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let has_rust = ecosystems.iter().any(|e| e.contains("rust"));
        let has_node = ecosystems.iter().any(|e| e.contains("node"));
        let has_python = ecosystems.iter().any(|e| e.contains("python"));
        let has_go = ecosystems.iter().any(|e| e.contains("go"));
        let has_java = ecosystems.iter().any(|e| e.contains("java"));

        match topic {
            OnboardingTopic::Tests => {
                let mut patterns = Vec::new();
                if has_rust {
                    patterns.push(r"(?i)\bcargo\s+test\b");
                }
                if has_node {
                    patterns.push(
                        r"(?i)\bnpm\s+test\b|\bpnpm\s+test\b|\byarn\s+test\b|\bvitest\b|\bjest\b",
                    );
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
        let grep_content_max_chars = grep_context_content_budget(grep_max_chars, response_mode);
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
