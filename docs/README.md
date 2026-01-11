# Documentation

This folder contains human-oriented documentation for Context.

If you are integrating Context programmatically, treat the **contracts** as the source of truth:

- [contracts/README.md](../contracts/README.md)
- [contracts/command/v1/](../contracts/command/v1/) (JSON Schemas)
- [contracts/http/v1/openapi.json](../contracts/http/v1/openapi.json) (OpenAPI 3.1)
- [proto/](../proto/) (gRPC)

## Start here

- [docs/AGENT_MEMORY.md](AGENT_MEMORY.md) — the “project memory” playbook (`read_pack` as the daily default)
- [docs/AGENT_DX_RUNBOOK.md](AGENT_DX_RUNBOOK.md) — multi-session hygiene, trust checks, and troubleshooting patterns
- [docs/QUICK_START.md](QUICK_START.md) — install, models, CLI, HTTP/gRPC, JSON API examples
- [USAGE_EXAMPLES.md](../USAGE_EXAMPLES.md) — agent-first workflows (best defaults and patterns)
- [docs/QUALITY_CHARTER.md](QUALITY_CHARTER.md) — premium-quality invariants, SLOs, and release gates (prevents regressions)
- [docs/EVALUATION.md](EVALUATION.md) — evaluation loop, metrics, and how we prevent quality regressions
- [docs/ARCHITECTURE.md](ARCHITECTURE.md) — crate map + data flow + on-disk layout
- [PHILOSOPHY.md](../PHILOSOPHY.md) — why the project is contract-first and agent-first

## API references (prose)

- [docs/COMMAND_RFC.md](COMMAND_RFC.md) — Command API overview (links to canonical schemas)
- [docs/CONTEXT_PACK.md](CONTEXT_PACK.md) — Context Pack v1 overview (links to canonical schema)
- [docs/MEANING_MODE_PLAN.md](MEANING_MODE_PLAN.md) — meanings-first context roadmap (Meaning Graph + Evidence + token-efficient Cognitive Pack)
- MCP: tool schemas live in [crates/mcp-server/src/tools/schemas/](../crates/mcp-server/src/tools/schemas/) and are dispatched via [crates/mcp-server/src/tools/dispatch/](../crates/mcp-server/src/tools/dispatch/); [crates/mcp-server/src/tools/mod.rs](../crates/mcp-server/src/tools/mod.rs) is the assembly entrypoint (wires schemas + routing)

## Contribution / dev workflow

- [AGENTS.md](../AGENTS.md) — rules for AI-agent-driven development in this repo
- [CONTRIBUTING.md](../CONTRIBUTING.md) — human contribution guide
