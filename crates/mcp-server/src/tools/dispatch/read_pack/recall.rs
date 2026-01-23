use std::path::Path;

use super::is_disallowed_memory_file;

pub(super) fn parse_path_token(token: &str) -> Option<(String, Option<usize>)> {
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

pub(super) fn extract_existing_file_ref(
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
pub(super) enum OpsIntent {
    TestAndGates,
    Snapshots,
    Run,
    Build,
    Deploy,
    Setup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RecallStructuralIntent {
    ProjectIdentity,
    EntryPoints,
    Contracts,
    Configuration,
}

pub(super) fn recall_structural_intent(question: &str) -> Option<RecallStructuralIntent> {
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

pub(super) fn recall_doc_candidate_score(rel: &str) -> i32 {
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
