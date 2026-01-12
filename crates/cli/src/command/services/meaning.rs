use crate::command::domain::{
    parse_payload, CommandOutcome, EvidenceFetchItem, EvidenceFetchOutput, EvidencePointer,
    MeaningFocusPayload, MeaningPackBudget, MeaningPackOutput, MeaningPackPayload,
    EVIDENCE_FETCH_VERSION, MEANING_PACK_VERSION,
};
use crate::command::warm;
use crate::command::{Hint, HintKind};
use anyhow::{anyhow, Context as AnyhowContext, Result};
use context_protocol::{enforce_max_chars, BudgetTruncation};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use context_meaning as meaning;

#[derive(Default)]
pub struct MeaningService;

impl MeaningService {
    pub async fn meaning_pack(
        &self,
        payload: serde_json::Value,
        ctx: &crate::command::context::CommandContext,
    ) -> Result<CommandOutcome> {
        const DEFAULT_MAX_CHARS: usize = 2_000;
        const MIN_MAX_CHARS: usize = 800;
        const MAX_MAX_CHARS: usize = 200_000;

        let payload: MeaningPackPayload = parse_payload(payload)?;
        let project_ctx = ctx.resolve_project(payload.project).await?;
        let _ = crate::heartbeat::ping(&project_ctx.root).await;
        let warm = warm::global_warmer().prewarm(&project_ctx.root).await;

        let max_chars = payload
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);

        let root_display = project_ctx.root.display().to_string();
        let root_fp = context_indexer::root_fingerprint(&root_display);

        let engine_req = meaning::MeaningPackRequest {
            query: payload.query.clone(),
            map_depth: None,
            map_limit: None,
            max_chars: Some(max_chars),
        };
        let engine = meaning::meaning_pack(&project_ctx.root, &root_display, &engine_req).await?;

        let output = MeaningPackOutput {
            version: MEANING_PACK_VERSION,
            query: engine.query,
            format: engine.format,
            pack: engine.pack,
            budget: MeaningPackBudget {
                max_chars: engine.budget.max_chars,
                used_chars: engine.budget.used_chars,
                truncated: engine.budget.truncated,
                truncation: engine.budget.truncation,
            },
            next_actions: Vec::new(),
            meta: context_indexer::ToolMeta {
                index_state: None,
                root_fingerprint: Some(root_fp),
            },
        };

        let mut outcome = CommandOutcome::from_value(output)?;
        outcome.meta.config_path = project_ctx.config_path;
        outcome.meta.index_updated = Some(false);
        outcome.meta.warm = Some(warm.warmed);
        outcome.meta.warm_cost_ms = Some(warm.warm_cost_ms);
        outcome.meta.warm_graph_cache_hit = Some(warm.graph_cache_hit);
        outcome.hints.extend(project_ctx.hints);
        outcome.hints.push(Hint {
            kind: HintKind::Info,
            text: "Meaning pack generated from deterministic meaning engine (lenses-first)."
                .to_string(),
        });
        Ok(outcome)
    }

    pub async fn meaning_focus(
        &self,
        payload: serde_json::Value,
        ctx: &crate::command::context::CommandContext,
    ) -> Result<CommandOutcome> {
        const DEFAULT_MAX_CHARS: usize = 2_000;
        const MIN_MAX_CHARS: usize = 800;
        const MAX_MAX_CHARS: usize = 200_000;

        let payload: MeaningFocusPayload = parse_payload(payload)?;
        let project_ctx = ctx.resolve_project(payload.project).await?;
        let _ = crate::heartbeat::ping(&project_ctx.root).await;
        let warm = warm::global_warmer().prewarm(&project_ctx.root).await;

        let max_chars = payload
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);

        let query = payload
            .query
            .clone()
            .unwrap_or_else(|| format!("focus: {}", payload.focus.trim()));

        let root_display = project_ctx.root.display().to_string();
        let root_fp = context_indexer::root_fingerprint(&root_display);

        let engine_req = meaning::MeaningFocusRequest {
            focus: payload.focus,
            query: Some(query.clone()),
            map_depth: None,
            map_limit: None,
            max_chars: Some(max_chars),
        };
        let engine = meaning::meaning_focus(&project_ctx.root, &root_display, &engine_req).await?;

        let output = MeaningPackOutput {
            version: MEANING_PACK_VERSION,
            query,
            format: engine.format,
            pack: engine.pack,
            budget: MeaningPackBudget {
                max_chars: engine.budget.max_chars,
                used_chars: engine.budget.used_chars,
                truncated: engine.budget.truncated,
                truncation: engine.budget.truncation,
            },
            next_actions: Vec::new(),
            meta: context_indexer::ToolMeta {
                index_state: None,
                root_fingerprint: Some(root_fp),
            },
        };

        let mut outcome = CommandOutcome::from_value(output)?;
        outcome.meta.config_path = project_ctx.config_path;
        outcome.meta.index_updated = Some(false);
        outcome.meta.warm = Some(warm.warmed);
        outcome.meta.warm_cost_ms = Some(warm.warm_cost_ms);
        outcome.meta.warm_graph_cache_hit = Some(warm.graph_cache_hit);
        outcome.hints.extend(project_ctx.hints);
        outcome.hints.push(Hint {
            kind: HintKind::Info,
            text: "Meaning focus generated from deterministic meaning engine (lenses-first)."
                .to_string(),
        });
        Ok(outcome)
    }

    pub async fn evidence_fetch(
        &self,
        payload: serde_json::Value,
        ctx: &crate::command::context::CommandContext,
    ) -> Result<CommandOutcome> {
        const DEFAULT_MAX_CHARS: usize = 8_000;
        const MIN_MAX_CHARS: usize = 800;
        const MAX_MAX_CHARS: usize = 200_000;
        const DEFAULT_MAX_LINES: usize = 200;
        const MAX_MAX_LINES: usize = 5_000;

        let payload: crate::command::domain::EvidenceFetchPayload = parse_payload(payload)?;
        let project_ctx = ctx.resolve_project(payload.project).await?;
        let _ = crate::heartbeat::ping(&project_ctx.root).await;
        let warm = warm::global_warmer().prewarm(&project_ctx.root).await;

        let max_chars = payload
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);
        let max_lines = payload
            .max_lines
            .unwrap_or(DEFAULT_MAX_LINES)
            .clamp(1, MAX_MAX_LINES);
        let strict_hash = payload.strict_hash.unwrap_or(false);

        let root_display = project_ctx.root.display().to_string();
        let root_fp = context_indexer::root_fingerprint(&root_display);

        let mut items = Vec::new();
        for mut evidence in payload.items {
            let rel = evidence.file.trim();
            if rel.is_empty() {
                return Err(anyhow!("Evidence file path must not be empty"));
            }
            let rel = rel.replace('\\', "/");
            if is_potential_secret_path(&rel) {
                return Err(anyhow!(
                    "Refusing to read potential secret path: {rel} (use file_slice with allow_secrets=true if you really mean it)"
                ));
            }

            let canonical = project_ctx
                .root
                .join(Path::new(&rel))
                .canonicalize()
                .with_context(|| format!("Failed to resolve evidence path '{rel}'"))?;
            if !canonical.starts_with(&project_ctx.root) {
                return Err(anyhow!("Evidence file '{rel}' is outside project root"));
            }
            let display_rel = normalize_relative_path(&project_ctx.root, &canonical).unwrap_or(rel);

            let (hash, file_lines) = hash_and_count_lines(&canonical).await?;
            let stale = evidence
                .source_hash
                .as_deref()
                .map(|expected| expected != hash)
                .unwrap_or(false);
            if stale && strict_hash {
                return Err(anyhow!(
                    "Evidence source_hash mismatch for {display_rel} (expected={}, got={hash})",
                    evidence.source_hash.as_deref().unwrap_or("<missing>")
                ));
            }

            evidence.file = display_rel.clone();
            evidence.source_hash = Some(hash.clone());

            let start_line = evidence.start_line.max(1);
            let end_line = evidence.end_line.max(start_line).min(file_lines.max(1));
            let (content, truncated) =
                read_file_lines_window(&canonical, start_line, end_line, max_lines).await?;

            let file_lc = display_rel.to_ascii_lowercase();
            let is_compose = file_lc.ends_with("docker-compose.yml")
                || file_lc.ends_with("docker-compose.yaml")
                || file_lc.ends_with("compose.yml")
                || file_lc.ends_with("compose.yaml");
            if is_compose && contains_potential_secret_assignment(&content) {
                return Err(anyhow!(
                    "Refusing to return potential secret snippet from {display_rel} (use file_slice with allow_secrets=true if you really mean it)"
                ));
            }

            items.push(EvidenceFetchItem {
                evidence: EvidencePointer {
                    start_line,
                    end_line,
                    ..evidence
                },
                content,
                truncated,
                stale,
            });
        }

        let mut output = EvidenceFetchOutput {
            version: EVIDENCE_FETCH_VERSION,
            items,
            budget: crate::command::domain::EvidenceFetchBudget {
                max_chars,
                used_chars: 0,
                truncated: false,
                truncation: None,
            },
            next_actions: Vec::new(),
            meta: context_indexer::ToolMeta {
                index_state: None,
                root_fingerprint: Some(root_fp),
            },
        };

        trim_evidence_fetch_to_budget(&mut output)?;

        let mut outcome = CommandOutcome::from_value(output)?;
        outcome.meta.config_path = project_ctx.config_path;
        outcome.meta.index_updated = Some(false);
        outcome.meta.warm = Some(warm.warmed);
        outcome.meta.warm_cost_ms = Some(warm.warm_cost_ms);
        outcome.meta.warm_graph_cache_hit = Some(warm.graph_cache_hit);
        outcome.hints.extend(project_ctx.hints);
        Ok(outcome)
    }
}

fn normalize_relative_path(root: &Path, canonical: &Path) -> Option<String> {
    let rel = canonical.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().into_owned().replace('\\', "/"))
}

fn is_potential_secret_path(candidate: &str) -> bool {
    let file_name = Path::new(candidate)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();

    match file_name.as_str() {
        ".env" | ".envrc" | ".npmrc" | ".pnpmrc" | ".yarnrc" | ".yarnrc.yml" | ".pypirc"
        | ".netrc" | "id_rsa" | "id_ed25519" | "id_ecdsa" | "id_dsa" => return true,
        _ => {}
    }

    if file_name.starts_with(".env.") {
        match file_name.as_str() {
            ".env.example" | ".env.sample" | ".env.template" | ".env.dist" => {}
            _ => return true,
        }
    }

    let normalized = candidate.replace('\\', "/").to_lowercase();
    if normalized == ".cargo/credentials"
        || normalized == ".cargo/credentials.toml"
        || normalized.ends_with("/.cargo/credentials")
        || normalized.ends_with("/.cargo/credentials.toml")
    {
        return true;
    }

    let ext = Path::new(candidate)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_lowercase();
    matches!(ext.as_str(), "pem" | "key" | "p12" | "pfx" | "env")
}

fn contains_potential_secret_assignment(content: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "password",
        "passwd",
        "token",
        "secret",
        "api_key",
        "apikey",
        "access_key",
        "secret_key",
        "client_secret",
        "private_key",
    ];

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') || line.starts_with("//") {
            continue;
        }

        let lc = line.to_lowercase();
        if !KEYWORDS.iter().any(|kw| lc.contains(kw)) {
            continue;
        }

        let (key, value) = if let Some((k, v)) = line.split_once(':') {
            (k.trim(), v.trim())
        } else if let Some((k, v)) = line.split_once('=') {
            (k.trim(), v.trim())
        } else {
            continue;
        };

        if key.is_empty() || value.is_empty() {
            continue;
        }

        if value.contains("${")
            || value.contains("$(")
            || value.eq_ignore_ascii_case("null")
            || value.eq_ignore_ascii_case("none")
            || value.eq_ignore_ascii_case("redacted")
            || value.eq_ignore_ascii_case("<redacted>")
        {
            continue;
        }
        if value.contains("changeme") || value.contains("example") {
            continue;
        }

        return true;
    }

    false
}

async fn hash_and_count_lines(path: &Path) -> Result<(String, usize)> {
    let meta = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("Failed to stat {}", path.display()))?;
    let file_size = meta.len();

    let mut file = File::open(path)
        .await
        .with_context(|| format!("Failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut newlines = 0usize;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        newlines += buf[..n].iter().filter(|&&b| b == b'\n').count();
    }
    let hash = format!("{:x}", hasher.finalize());
    let lines = if file_size == 0 { 0 } else { newlines + 1 };
    Ok((hash, lines))
}

async fn read_file_lines_window(
    path: &Path,
    start_line: usize,
    end_line: usize,
    max_lines: usize,
) -> Result<(String, bool)> {
    let file = File::open(path)
        .await
        .with_context(|| format!("Failed to open {}", path.display()))?;
    let mut reader = BufReader::new(file).lines();

    let mut current = 0usize;
    let mut out: Vec<String> = Vec::new();
    let mut truncated = false;
    while let Some(line) = reader.next_line().await? {
        current += 1;
        if current < start_line {
            continue;
        }
        if current > end_line {
            break;
        }
        out.push(line);
        if out.len() >= max_lines {
            truncated = true;
            break;
        }
    }
    Ok((out.join("\n"), truncated))
}

fn trim_evidence_fetch_to_budget(output: &mut EvidenceFetchOutput) -> Result<()> {
    let max_chars = output.budget.max_chars;
    let used = enforce_max_chars(
        output,
        max_chars,
        |inner, used| inner.budget.used_chars = used,
        |inner| {
            inner.budget.truncated = true;
            inner.budget.truncation = Some(BudgetTruncation::MaxChars);
        },
        |inner| {
            if inner.items.len() > 1 {
                inner.items.pop();
                return true;
            }
            if let Some(item) = inner.items.first_mut() {
                if item.content.is_empty() {
                    return false;
                }
                item.truncated = true;
                let target = item.content.chars().count().saturating_sub(200);
                item.content = item.content.chars().take(target).collect::<String>();
                return true;
            }
            false
        },
    )?;
    output.budget.used_chars = used;
    Ok(())
}
