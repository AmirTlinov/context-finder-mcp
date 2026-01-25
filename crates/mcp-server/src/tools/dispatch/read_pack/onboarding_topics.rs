use super::cursors::trimmed_non_empty_str;
use super::{ProjectFactsResult, ReadPackRequest};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OnboardingTopic {
    Tests,
    Run,
    Build,
    Install,
    CI,
    Structure,
    Unknown,
}

pub(super) fn onboarding_prompt(request: &ReadPackRequest) -> String {
    let mut parts = Vec::new();
    if let Some(text) = trimmed_non_empty_str(request.ask.as_deref()) {
        parts.push(text.to_string());
    }
    if let Some(text) = trimmed_non_empty_str(request.query.as_deref()) {
        parts.push(text.to_string());
    }
    if let Some(questions) = request.questions.as_ref() {
        for question in questions {
            if let Some(text) = trimmed_non_empty_str(Some(question)) {
                parts.push(text.to_string());
            }
        }
    }
    parts.join("\n")
}

pub(super) fn classify_onboarding_topic(prompt: &str) -> OnboardingTopic {
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

pub(super) fn command_grep_pattern(
    topic: OnboardingTopic,
    facts: &ProjectFactsResult,
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
                patterns.push(r"(?i)\bnpm\s+run\s+build\b|\bpnpm\s+run\s+build\b|\byarn\s+build\b");
            }
            if has_python {
                patterns.push(r"(?i)\bpython\s+-m\s+build\b|\bpoetry\s+build\b");
            }
            if has_go {
                patterns.push(r"(?i)\bgo\s+build\b");
            }
            if has_java {
                patterns.push(r"(?i)\bmvn\s+package\b|\bgradle\b.*\bbuild\b");
            }
            if patterns.is_empty() {
                patterns.push(r"(?i)\bcargo\s+build\b|\bnpm\s+run\s+build\b|\bgo\s+build\b");
            }
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
                patterns.push(r"(?i)\bgo\s+install\b");
            }
            if has_java {
                patterns.push(r"(?i)\bmvn\s+install\b|\bgradle\b.*\binstall\b");
            }
            if patterns.is_empty() {
                patterns.push(r"(?i)\bnpm\s+install\b|\bpip\s+install\b");
            }
            Some(patterns.join("|"))
        }
        OnboardingTopic::CI => Some(
            r"(?i)\bgithub\s+actions\b|\bci\b|\bcircleci\b|\bbuildkite\b|\bjenkins\b".to_string(),
        ),
        OnboardingTopic::Structure => None,
        OnboardingTopic::Unknown => None,
    }
}

pub(super) fn onboarding_doc_candidates(topic: OnboardingTopic) -> Vec<&'static str> {
    let mut out = vec!["AGENTS.md", "README.md", "docs/QUICK_START.md"];
    match topic {
        OnboardingTopic::Tests => {
            out.extend(["docs/README.md", "CONTRIBUTING.md", "USAGE_EXAMPLES.md"]);
        }
        OnboardingTopic::Run => {
            out.extend(["USAGE_EXAMPLES.md", "docs/README.md", "README.md"]);
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
