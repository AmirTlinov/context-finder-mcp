use serde_json::json;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ToolDescriptor {
    pub(crate) name: &'static str,
    pub(crate) summary: &'static str,
}

pub(crate) const TOOL_CATALOG: &[ToolDescriptor] = &[
    ToolDescriptor {
        name: "capabilities",
        summary: "Handshake: versions, default budgets, and start route.",
    },
    ToolDescriptor {
        name: "help",
        summary: "Explain the `.context` legend (A/R/N/M) and best practices.",
    },
    ToolDescriptor {
        name: "repo_onboarding_pack",
        summary: "Map + key docs + next_actions (best first call).",
    },
    ToolDescriptor {
        name: "read_pack",
        summary: "One-call file/grep/query/onboarding with cursor-only continuation.",
    },
    ToolDescriptor {
        name: "context_pack",
        summary: "Bounded semantic pack (primary + related halo).",
    },
    ToolDescriptor {
        name: "batch",
        summary: "Multiple tools under one max_chars budget with $ref.",
    },
    ToolDescriptor {
        name: "map",
        summary: "Project structure overview (directories + symbols).",
    },
    ToolDescriptor {
        name: "list_files",
        summary: "Bounded file enumeration (glob/substring filter).",
    },
    ToolDescriptor {
        name: "file_slice",
        summary: "Bounded file window (root-locked, hashed).",
    },
    ToolDescriptor {
        name: "grep_context",
        summary: "Regex matches with before/after context hunks.",
    },
    ToolDescriptor {
        name: "text_search",
        summary: "Fast text search (corpus, optional FS fallback).",
    },
    ToolDescriptor {
        name: "search",
        summary: "Semantic search (fast, index-backed).",
    },
    ToolDescriptor {
        name: "context",
        summary: "Semantic search with graph-aware context.",
    },
    ToolDescriptor {
        name: "impact",
        summary: "Find symbol usages and transitive impact.",
    },
    ToolDescriptor {
        name: "trace",
        summary: "Call chain between two symbols.",
    },
    ToolDescriptor {
        name: "explain",
        summary: "Symbol details, deps, dependents, docs.",
    },
    ToolDescriptor {
        name: "overview",
        summary: "Architecture snapshot (layers, entry points).",
    },
    ToolDescriptor {
        name: "doctor",
        summary: "Diagnostics for model/GPU/index state.",
    },
];

pub(crate) fn tool_inventory_json(version: &str) -> serde_json::Value {
    let tools: Vec<serde_json::Value> = TOOL_CATALOG
        .iter()
        .map(|tool| json!({ "name": tool.name, "summary": tool.summary }))
        .collect();

    json!({
        "binary": "context-mcp",
        "version": version,
        "count": tools.len(),
        "tools": tools,
    })
}

pub(crate) fn tool_instructions() -> String {
    let mut lines = vec![
        "Context provides semantic code search for AI agents.".to_string(),
        "Recommended flow: repo_onboarding_pack → read_pack → context_pack; use batch for multi-step queries."
            .to_string(),
        "Use help for the `.context` legend (A/R/N/M).".to_string(),
        "Tools:".to_string(),
    ];
    for tool in TOOL_CATALOG {
        lines.push(format!("- {}: {}", tool.name, tool.summary));
    }
    lines.join("\n")
}
