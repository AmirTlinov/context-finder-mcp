# Contracts

This directory is the **source of truth** for Context’s external interfaces.
Anything that can be called by another process (CLI JSON, HTTP, gRPC, MCP tools) must be specified
here first, then implemented in code.

## Principles

- **Contract-first:** update the contract before touching implementation.
- **Machine-readable:** contracts are JSON Schema / OpenAPI / `.proto`, not prose.
- **Versioned:** each surface lives under `contracts/<surface>/vN/` where `N` is a compatibility line.
- **No silent breaking changes:** removals/renames/semantic shifts require a new `v(N+1)`.

## Surfaces

### Command API (JSON envelope)

**What:** the stable request/response envelope used by:

- `context command --json '{...}'`
- `context serve-http` (`POST /command`)
- `context serve-grpc` (JSON payload passthrough; see `proto/command.proto`)

**Contracts (v1):**

- `contracts/command/v1/command_request.schema.json`
- `contracts/command/v1/command_response.schema.json`
- `contracts/command/v1/error.schema.json` (error envelope)
- `contracts/command/v1/next_action.schema.json` (next-step tool calls)
- `contracts/command/v1/budget_truncation.schema.json` (budget truncation enum)
- `contracts/command/v1/batch.schema.json` (schema for `payload`/`data` when action is `batch`)
- `contracts/command/v1/capabilities.schema.json` (schema for `data` when action is `capabilities`)
- `contracts/command/v1/context_pack.schema.json` (schema for `data` when action is `context_pack`)
- `contracts/command/v1/task_pack.schema.json` (schema for `data` when action is `task_pack`)
- `contracts/command/v1/text_search.schema.json` (schema for `data` when action is `text_search`)

### Agent Notebook (durable anchors + runbooks)

**What:** a durable, agent-authored knowledge layer for cross-session continuity:

- Notebook anchors (“hot spots”) with evidence pointers.
- Runbooks that refresh specific subsystems with **fresh/stale** truthfulness.

**Contracts (v1):**

- `contracts/agent/v1/notebook.schema.json`
- `contracts/agent/v1/notebook_anchor.schema.json`
- `contracts/agent/v1/notebook_evidence_pointer.schema.json`
- `contracts/agent/v1/notebook_edit.schema.json`
- `contracts/agent/v1/notebook_apply_suggest.schema.json`
- `contracts/agent/v1/notebook_apply_suggest_result.schema.json`
- `contracts/agent/v1/notebook_pack.schema.json`
- `contracts/agent/v1/notebook_suggest.schema.json`
- `contracts/agent/v1/runbook.schema.json`
- `contracts/agent/v1/runbook_section.schema.json`
- `contracts/agent/v1/runbook_pack.schema.json`

### Evaluation outputs (local, optional)

**What:** machine-readable outputs produced by local evaluation runners (e.g. real-repo zoo). These
are intended for trend tracking and regression triage. CI does not require them by default.

**Contracts (v1):**

- `contracts/eval/v1/zoo_report.schema.json` (schema for `context-mcp-eval-zoo --out-json ...`)
- `contracts/command/v1/request_options.schema.json` (cross-cutting options: freshness policy, filters, budgets)
- `contracts/command/v1/index_state.schema.json` (response diagnostics: watermarks + stale reasons + auto-index metadata)
- `contracts/command/v1/watermark.schema.json` (git/fs watermark primitive)
- `contracts/command/v1/health_report.schema.json` (HTTP `GET /health` response)

Primary code source:

- `crates/cli/src/command/domain.rs` (envelope + enums)
- `crates/search/src/context_pack.rs` (Context Pack v1)
- `crates/cli/src/command/infra/health.rs` (Health report)

### HTTP API

**What:** `context serve-http` HTTP surface.

**Contract (v1):**

- `contracts/http/v1/openapi.json` (OpenAPI 3.1; references the JSON Schemas above)

Primary code source:

- `crates/cli/src/lib.rs` (`serve_http`, `/command`, `/health`)

### gRPC API

**What:** `context serve-grpc` gRPC surface.

**Contracts:**

- `proto/command.proto` (gRPC service with JSON passthrough + health)
- `proto/contextfinder.proto` (typed gRPC surface; payload/config use `google.protobuf.Struct`)

### MCP tools

**What:** MCP server for agent integration.

**Contract source:**

- Tool schemas: `crates/mcp-server/src/tools/schemas/`
- Dispatch/routing: `crates/mcp-server/src/tools/dispatch/`
- Assembly entrypoint: `crates/mcp-server/src/tools/mod.rs`

Key agent-oriented tools (MCP):

- `repo_onboarding_pack` — one call returns `tree` + key docs slices + `next_actions` under one `max_chars` budget.
- `rg` — regex context reads (grep `-B/-A/-C`) with merged hunks, explicit budgets, and `next_cursor` pagination.
- `cat` — bounded file reads (designed to replace `cat`/`sed` loops); supports `next_cursor` pagination for large files.
- `read_pack` — one-call “semantic reading” facade: returns `cat` / `rg` / `context_pack` / `repo_onboarding_pack` results as `sections[]` under one budget; supports cursor-only continuation for file/grep.
- `batch` — one-call orchestration; batch `version: 2` (default) supports `$ref` (JSON Pointer) + optional `$default` for light templating between items.

Large outputs: `tree`, `ls`, `text_search`, `rg`, `cat` can return `next_cursor` so callers can page without relying on truncation heuristics.

## Change workflow (human + AI agents)

1. Edit/add the contract under `contracts/…` (and/or `proto/…`).
2. Update implementation.
3. Add/adjust tests.
4. Run checks:
   - `scripts/validate_contracts.sh`
   - `CONTEXT_FINDER_EMBEDDING_MODE=stub cargo test --workspace`
5. Update prose docs (`docs/…`) only to explain and link back to the contract.
