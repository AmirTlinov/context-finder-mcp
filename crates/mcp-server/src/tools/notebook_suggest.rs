use anyhow::Result;
use context_indexer::ToolMeta;
use context_protocol::ToolNextAction;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::cpv1::{parse_cpv1_dict, parse_cpv1_evidence};
use super::meaning_pack::compute_meaning_pack_result;
use super::notebook_store::resolve_repo_identity;
use super::notebook_types::{
    AgentRunbook, NotebookAnchor, NotebookAnchorKind, NotebookEvidencePointer, NotebookScope,
    RunbookPolicy, RunbookSection,
};
use super::schemas::meaning_pack::MeaningPackRequest;
use super::schemas::notebook_suggest::{
    NotebookSuggestBudget, NotebookSuggestRequest, NotebookSuggestResult,
};
use super::schemas::response_mode::ResponseMode;
use super::util::hex_encode_lower;

const VERSION: u32 = 1;
const DEFAULT_MAX_CHARS: usize = 2_000;
const MIN_MAX_CHARS: usize = 800;
const MAX_MAX_CHARS: usize = 500_000;

const DEFAULT_QUERY: &str = "entrypoints contracts ci gates worktrees";

#[derive(Debug, Clone)]
struct Cpv1AnchorLine {
    kind: String,
    label: String,
    ev: String,
}

fn clamp_max_chars(max_chars: Option<usize>) -> usize {
    max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS)
}

fn normalize_query(query: Option<&str>) -> String {
    let q = query.unwrap_or(DEFAULT_QUERY).trim();
    if q.is_empty() {
        DEFAULT_QUERY.to_string()
    } else {
        q.to_string()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode_lower(&hasher.finalize())
}

fn stable_anchor_id(kind: &NotebookAnchorKind, file: &str) -> String {
    let kind_prefix = match kind {
        NotebookAnchorKind::Canon => "canon",
        NotebookAnchorKind::Ci => "ci",
        NotebookAnchorKind::Contract => "contract",
        NotebookAnchorKind::Entrypoint => "entrypoint",
        NotebookAnchorKind::Zone => "zone",
        NotebookAnchorKind::Work => "work",
        NotebookAnchorKind::Other => "other",
    };
    let key = format!("{kind_prefix}|{file}");
    let hash = sha256_hex(key.as_bytes());
    format!("{kind_prefix}_{}", &hash[..12.min(hash.len())])
}

fn stable_runbook_id(slug: &str) -> String {
    let key = format!("runbook|{slug}");
    let hash = sha256_hex(key.as_bytes());
    format!("rb_{}_{}", slug, &hash[..12.min(hash.len())])
}

fn map_anchor_kind(kind: &str) -> NotebookAnchorKind {
    match kind.trim().to_ascii_lowercase().as_str() {
        "canon" => NotebookAnchorKind::Canon,
        "ci" => NotebookAnchorKind::Ci,
        "contract" => NotebookAnchorKind::Contract,
        "entrypoint" => NotebookAnchorKind::Entrypoint,
        "zone" => NotebookAnchorKind::Zone,
        "work" => NotebookAnchorKind::Work,
        _ => NotebookAnchorKind::Other,
    }
}

fn parse_cpv1_anchor_lines(pack: &str, dict: &HashMap<String, String>) -> Vec<Cpv1AnchorLine> {
    let mut out: Vec<Cpv1AnchorLine> = Vec::new();
    for raw in pack.lines() {
        let line = raw.trim_end_matches('\r');
        if !line.starts_with("ANCHOR ") {
            continue;
        }
        let mut kind: Option<String> = None;
        let mut label: Option<String> = None;
        let mut ev: Option<String> = None;
        for token in line.split_whitespace() {
            if let Some(v) = token.strip_prefix("kind=") {
                kind = Some(v.to_string());
                continue;
            }
            if let Some(v) = token.strip_prefix("label=") {
                let resolved = dict.get(v).cloned().unwrap_or_else(|| v.to_string());
                label = Some(resolved);
                continue;
            }
            if let Some(v) = token.strip_prefix("ev=") {
                ev = Some(v.to_string());
                continue;
            }
        }
        let (Some(kind), Some(label), Some(ev)) = (kind, label, ev) else {
            continue;
        };
        out.push(Cpv1AnchorLine { kind, label, ev });
    }
    out
}

fn build_suggested_runbooks(
    anchors: &[NotebookAnchor],
    include_worktrees: bool,
    query: &str,
) -> Vec<AgentRunbook> {
    let mut by_kind: HashMap<NotebookAnchorKind, Vec<String>> = HashMap::new();
    for a in anchors {
        by_kind
            .entry(a.kind.clone())
            .or_default()
            .push(a.id.clone());
    }

    let mut portal_anchor_ids: Vec<String> = Vec::new();
    for kind in [
        NotebookAnchorKind::Canon,
        NotebookAnchorKind::Contract,
        NotebookAnchorKind::Ci,
        NotebookAnchorKind::Entrypoint,
        NotebookAnchorKind::Zone,
    ] {
        if let Some(ids) = by_kind.get(&kind) {
            if let Some(first) = ids.first() {
                portal_anchor_ids.push(first.clone());
            }
        }
    }

    let portal_policy = RunbookPolicy {
        default_mode: super::notebook_types::RunbookDefaultMode::Summary,
        noise_budget: 0.2,
        max_items_per_section: 8,
    };

    let mut portal_sections: Vec<RunbookSection> = Vec::new();
    if include_worktrees {
        portal_sections.push(RunbookSection::Worktrees {
            id: "s_worktrees".to_string(),
            title: "Worktrees / branches".to_string(),
            max_chars: Some(1_200),
            limit: Some(40),
        });
    }
    if !portal_anchor_ids.is_empty() {
        portal_sections.push(RunbookSection::Anchors {
            id: "s_hotspots".to_string(),
            title: "Hot spots (pointers + staleness)".to_string(),
            anchor_ids: portal_anchor_ids.clone(),
            include_evidence: false,
        });
    }
    portal_sections.push(RunbookSection::MeaningPack {
        id: "s_meaning".to_string(),
        title: "Meaning map (canon + boundaries)".to_string(),
        query: query.to_string(),
        max_chars: Some(1_400),
    });

    let mut out: Vec<AgentRunbook> = Vec::new();
    out.push(AgentRunbook {
        id: stable_runbook_id("daily_portal"),
        title: "Daily portal".to_string(),
        purpose: "Low-noise refresh of what matters: canon loop, key anchors, and worktrees."
            .to_string(),
        policy: portal_policy,
        sections: portal_sections,
        created_at: None,
        updated_at: None,
    });

    let mut contracts_ci_ids: Vec<String> = Vec::new();
    for kind in [NotebookAnchorKind::Contract, NotebookAnchorKind::Ci] {
        if let Some(ids) = by_kind.get(&kind) {
            contracts_ci_ids.extend(ids.iter().take(2).cloned());
        }
    }
    if !contracts_ci_ids.is_empty() {
        out.push(AgentRunbook {
            id: stable_runbook_id("contracts_ci"),
            title: "Contracts + CI".to_string(),
            purpose:
                "Evidence-backed refresh for contracts and CI gates (bounded by noise_budget)."
                    .to_string(),
            policy: RunbookPolicy {
                default_mode: super::notebook_types::RunbookDefaultMode::Summary,
                noise_budget: 0.35,
                max_items_per_section: 8,
            },
            sections: vec![RunbookSection::Anchors {
                id: "s_contracts_ci".to_string(),
                title: "Contracts + CI hot spots".to_string(),
                anchor_ids: contracts_ci_ids,
                include_evidence: true,
            }],
            created_at: None,
            updated_at: None,
        });
    }

    if include_worktrees {
        out.push(AgentRunbook {
            id: stable_runbook_id("worktrees"),
            title: "Worktrees overview".to_string(),
            purpose: "Bounded overview of active worktrees/branches (best for .worktrees repos)."
                .to_string(),
            policy: RunbookPolicy {
                default_mode: super::notebook_types::RunbookDefaultMode::Summary,
                noise_budget: 0.0,
                max_items_per_section: 1,
            },
            sections: vec![RunbookSection::Worktrees {
                id: "s_worktrees".to_string(),
                title: "Worktrees / branches".to_string(),
                max_chars: Some(1_400),
                limit: Some(80),
            }],
            created_at: None,
            updated_at: None,
        });
    }

    out
}

pub(super) async fn compute_notebook_suggest_result(
    root: &Path,
    root_display: &str,
    request: &NotebookSuggestRequest,
) -> Result<NotebookSuggestResult> {
    let response_mode = request.response_mode.unwrap_or(ResponseMode::Facts);
    let max_chars = clamp_max_chars(request.max_chars);
    let scope = request.scope.unwrap_or(NotebookScope::Project);
    let query = normalize_query(request.query.as_deref());

    // Generate evidence-backed suggestions using the meaning engine (no notebook I/O here).
    let mp_req = MeaningPackRequest {
        path: None,
        query: query.clone(),
        map_depth: None,
        map_limit: None,
        max_chars: Some(2_000),
        response_mode: Some(ResponseMode::Facts),
        output_format: None,
        auto_index: Some(true),
        auto_index_budget_ms: Some(15_000),
    };
    let pack = compute_meaning_pack_result(root, root_display, &mp_req).await?;
    let dict = parse_cpv1_dict(&pack.pack);
    let ev_by_id = parse_cpv1_evidence(&pack.pack, &dict);
    let anchor_lines = parse_cpv1_anchor_lines(&pack.pack, &dict);

    let mut anchors: Vec<NotebookAnchor> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    for a in anchor_lines.into_iter().take(10) {
        let kind = map_anchor_kind(&a.kind);
        let Some(ptr) = ev_by_id.get(&a.ev) else {
            continue;
        };
        let id = stable_anchor_id(&kind, &ptr.file);
        if !seen_ids.insert(id.clone()) {
            continue;
        }
        anchors.push(NotebookAnchor {
            id,
            kind,
            label: a.label,
            evidence: vec![NotebookEvidencePointer {
                file: ptr.file.clone(),
                start_line: ptr.start_line as u32,
                end_line: ptr.end_line as u32,
                source_hash: ptr.source_hash.clone(),
            }],
            locator: None,
            created_at: None,
            updated_at: None,
            tags: vec!["suggested".to_string()],
        });
    }

    let include_worktrees = root.join(".worktrees").is_dir();
    let runbooks = build_suggested_runbooks(&anchors, include_worktrees, &query);

    let mut next_actions: Vec<ToolNextAction> = Vec::new();
    if response_mode == ResponseMode::Full {
        // 1) Apply all suggested anchors + runbooks in a single explicit write.
        let mut ops: Vec<serde_json::Value> = Vec::new();
        for a in &anchors {
            ops.push(json!({ "op": "upsert_anchor", "anchor": a }));
        }
        for rb in &runbooks {
            ops.push(json!({ "op": "upsert_runbook", "runbook": rb }));
        }

        let mut edit_args = json!({
            "version": 1,
            "scope": match scope { NotebookScope::Project => "project", NotebookScope::UserRepo => "user_repo" },
            "ops": ops,
        });
        if let Some(path) = request.path.as_deref() {
            if let Some(obj) = edit_args.as_object_mut() {
                obj.insert("path".to_string(), json!(path));
            }
        }
        next_actions.push(ToolNextAction {
            tool: "notebook_edit".to_string(),
            args: edit_args,
            reason: "Persist suggested anchors + runbooks (explicit write).".to_string(),
        });

        // 2) After applying, run the daily portal in summary mode.
        if let Some(portal) = runbooks.first() {
            let mut args = json!({
                "runbook_id": portal.id,
                "mode": "summary",
                "scope": match scope { NotebookScope::Project => "project", NotebookScope::UserRepo => "user_repo" },
                "max_chars": 2000,
                "response_mode": "facts"
            });
            if let Some(path) = request.path.as_deref() {
                if let Some(obj) = args.as_object_mut() {
                    obj.insert("path".to_string(), json!(path));
                }
            }
            next_actions.push(ToolNextAction {
                tool: "runbook_pack".to_string(),
                args,
                reason: "Refresh the suggested daily portal (TOC-only by default).".to_string(),
            });
        }
    }

    let identity = resolve_repo_identity(root);
    Ok(NotebookSuggestResult {
        version: VERSION,
        repo_id: identity.repo_id,
        query,
        anchors,
        runbooks,
        budget: NotebookSuggestBudget {
            max_chars,
            used_chars: 0,
            truncated: false,
        },
        next_actions,
        meta: ToolMeta::default(),
    })
}
