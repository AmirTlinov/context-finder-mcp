# Release Train (Contracts-First Delivery)

This document defines how we ship Context changes without breaking agents, integrations, or trust.

The core principle is simple:

- **Contracts are the source of truth.**
- **Quality gates are non-negotiable.**
- **Behavioral deltas must be measurable and regression-proof.**

## 0) What counts as “external behavior”

Treat a change as external-facing if *anything* outside this repo could observe it:

- CLI JSON output
- Command API (HTTP, gRPC)
- MCP tool schemas and tool behavior (`.context` output shape/semantics)
- Contract files under `contracts/**` and `proto/**`

If in doubt: assume it is external.

## 1) Versioning policy (contracts-first)

### 1.1) Source of truth

- **Command envelope (JSON Schema):** `contracts/command/v1/*`
- **HTTP API (OpenAPI 3.1):** `contracts/http/v1/openapi.json`
- **gRPC:** `proto/*.proto`
- **MCP tool schemas:** `crates/mcp-server/src/tools/schemas/*`

### 1.2) Breaking change rule

Breaking changes require a new version line:

- JSON Schema / OpenAPI / gRPC: introduce `v(N+1)` in the canonical location (keep `vN` intact).
- MCP tool schemas: same rule — breaking schema changes require a new schema version and explicit wiring.

“Breaking” includes:

- removing or renaming fields
- changing field meaning/units
- changing default behavior in a way that breaks existing clients/automation

Non-breaking changes are typically additive (new optional fields, new tools, new optional output notes).

## 2) Changelog policy (delivery as product)

`CHANGELOG.md` is required for any observable behavior change.

Changelog entries must be **actionable** and **test-linked**:

- What changed (Added / Changed / Fixed)
- Which surface (MCP tool / CLI / HTTP / gRPC / contracts)
- How it is gated (test name, dataset, or quality gate)

We prefer short “behavioral deltas” over long prose.

## 3) Release checklist (train)

1) Ensure contracts are updated first (if any surface changes).
2) Update `CHANGELOG.md`:
   - move “Unreleased” changes into a dated version section
   - keep “Unreleased” empty (or remove it) after release
3) Run mandatory gates:
   - `scripts/validate_quality.sh` (includes contracts + fmt + clippy + stub tests + stub eval smoke + HTTP contract conformance)
4) Bump the workspace version (`[workspace.package].version` in the root `Cargo.toml`) if releasing.
5) Tag and publish the release (policy depends on the distribution channel).
6) Post-release sanity:
   - `docs/QUICK_START.md` reflects user-facing knobs and new safe defaults
   - `docs/EVALUATION.md` reflects any new evaluation datasets/gates

## 4) Compatibility promises (how we keep trust)

### 4.1) Agents (MCP)

- Default output stays **low-noise** (`response_mode=facts` / `minimal`).
- Any “guidance” additions must not require clients to parse structured JSON to function.
- When evidence is insufficient under budget, we emit **fewer claims**, not guesses.

### 4.2) Programmatic integrations (Command API)

- Machine-readable behavior is governed by the published contracts.
- Clients should pin contract versions and upgrade intentionally.

## 5) Rollback discipline

When risk is non-trivial:

- prefer **gated behavior** (flag/env/config) over silent global flips
- keep a clear “off switch” path
- treat regressions as release blockers (quality is a feature)
