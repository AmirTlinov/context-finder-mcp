# Documentation

This folder contains human-oriented documentation for Context Finder.

If you are integrating Context Finder programmatically, treat the **contracts** as the source of truth:

- [contracts/README.md](../contracts/README.md)
- [contracts/command/v1/](../contracts/command/v1/) (JSON Schemas)
- [contracts/http/v1/openapi.json](../contracts/http/v1/openapi.json) (OpenAPI 3.1)
- [proto/](../proto/) (gRPC)

## Start here

- [docs/QUICK_START.md](QUICK_START.md) — install, models, CLI, HTTP/gRPC, JSON API examples
- [USAGE_EXAMPLES.md](../USAGE_EXAMPLES.md) — agent-first workflows (best defaults and patterns)
- [docs/ARCHITECTURE.md](ARCHITECTURE.md) — crate map + data flow + on-disk layout
- [PHILOSOPHY.md](../PHILOSOPHY.md) — why the project is contract-first and agent-first

## API references (prose)

- [docs/COMMAND_RFC.md](COMMAND_RFC.md) — Command API overview (links to canonical schemas)
- [docs/CONTEXT_PACK.md](CONTEXT_PACK.md) — Context Pack v1 overview (links to canonical schema)
- MCP: tool contracts live in [crates/mcp-server/src/tools.rs](../crates/mcp-server/src/tools.rs) (includes `batch` for one-call multi-tool workflows)

## Contribution / dev workflow

- [AGENTS.md](../AGENTS.md) — rules for AI-agent-driven development in this repo
- [CONTRIBUTING.md](../CONTRIBUTING.md) — human contribution guide
