use super::candidates::{config_candidate_score, is_disallowed_memory_file};
use super::recall::{recall_doc_candidate_score, RecallStructuralIntent};
use super::{entrypoint_candidate_score, ProjectFactsResult};
use std::collections::HashSet;
use std::path::Path;

mod paths;
mod scores;

use paths::{
    CONFIG_DOC_HINTS, CONFIG_FILE_HINTS, CONTRACT_FRONT_DOOR_DOCS, CONTRACT_HINTS,
    ENTRYPOINT_HINTS, MODULE_DOC_HINTS, MODULE_ENTRYPOINT_HINTS, PROJECT_IDENTITY_DOCS,
};
use scores::contract_candidate_score;

pub(super) fn recall_structural_candidates(
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
            for rel in PROJECT_IDENTITY_DOCS {
                push(rel);
            }

            // If the root is a wrapper, surface module docs as well (bounded, deterministic).
            for module in facts.modules.iter().take(6) {
                for rel in MODULE_DOC_HINTS {
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
            for rel in ENTRYPOINT_HINTS {
                push(rel);
            }

            for rel in &facts.entry_points {
                push(rel);
            }

            // If project_facts didn't find module entrypoints, derive a few from module roots.
            for module in facts.modules.iter().take(12) {
                for rel in MODULE_ENTRYPOINT_HINTS {
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
            for rel in CONTRACT_HINTS {
                push(rel);
            }

            // If there are contract dirs, surface one or two stable "front door" docs from them.
            for module in facts
                .contracts
                .iter()
                .filter(|c| c.ends_with('/') || root.join(c).is_dir())
                .take(4)
            {
                for rel in CONTRACT_FRONT_DOOR_DOCS {
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
            for rel in CONFIG_DOC_HINTS {
                push(rel);
            }

            for rel in &facts.key_configs {
                push(rel);
            }

            for rel in CONFIG_FILE_HINTS {
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
