# Agent Development Rules (Context Finder)

This repository is built for **agent-first integrations**. To keep the system reliable and easy to evolve, all external behavior is managed **through contracts**.

## 1) Contract-first (mandatory)

If your change affects *anything* another process can call or parse (CLI JSON, HTTP, gRPC, MCP tools):

1. Update the contract first:
   - `contracts/…` (JSON Schema / OpenAPI)
   - and/or `proto/*.proto` (gRPC)
2. Implement the change.
3. Add/adjust tests.
4. Run checks (see “Quality gates”).
5. Update prose docs in `docs/…` to **link back** to the contract (do not duplicate the source of truth).

Breaking changes require a new `contracts/<surface>/v(N+1)/` line.

## 2) Source of truth map

- **Command envelope (JSON):** `contracts/command/v1/*`
- **HTTP API:** `contracts/http/v1/openapi.json`
- **gRPC:** `proto/command.proto`, `proto/contextfinder.proto`
- **MCP tool schemas:** `crates/mcp-server/src/tools/schemas/*` (wired via `crates/mcp-server/src/tools/dispatch/`; assembly: `crates/mcp-server/src/tools/mod.rs`)
- **Implementation of HTTP routes:** `crates/cli/src/main.rs` (`/command`, `/health`)
- **Command envelope Rust types:** `crates/cli/src/command/domain.rs`

### 2.1) Agent discovery defaults (prefer tools over shell)

When you need to *understand a repo* or *read lots of context*, prefer bounded MCP tools over ad-hoc `ls/rg/sed` loops:

- `repo_onboarding_pack` — first call in a new repo (map + key docs + next actions).
- `grep_context` — “grep with N lines before/after”, merged into hunks under strict budgets.
- `batch` with `version: 2` — chain tool calls with `$ref` (JSON Pointer) dependencies under one `max_chars`.
- Cursor pagination — if `next_cursor` is present, continue by repeating the call with `cursor`.

## 3) Development loop (fast, safe)

1. Discover the nearest existing pattern (do not invent new layers).
2. Make the smallest change that fixes the root cause.
3. Keep boundaries clean:
   - core logic lives in crates like `search/`, `indexer/`, `graph/`
   - adapters/entrypoints live in `cli/`, `mcp-server/`
4. Prefer deterministic test mode:
   - `CONTEXT_FINDER_EMBEDDING_MODE=stub`

## 4) Quality gates (must be green)

Run these before considering a change “done”:

```bash
scripts/validate_contracts.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
CONTEXT_EMBEDDING_MODE=stub cargo test --workspace
```

Notes:

- Model downloads are **not** required for most development.
- CPU fallback is **opt-in** only: `CONTEXT_ALLOW_CPU=1`.

## 5) Repository hygiene (hard rules)

- Do not commit downloaded model assets under `models/**`.
- Do not commit local caches (`.agents/mcp/context/.context/` (preferred), legacy `.context/` / `.context-finder/`, `.fastembed_cache/`, `.deps/`, etc.).
- Avoid churn: no formatting-only refactors, no mass renames.
- When you add a new public knob (flag/env/config), document it in:
  - the relevant contract (if it affects an API surface)
  - `docs/QUICK_START.md` (if user-facing)
