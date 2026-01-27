# Agent DX Runbook (Context)

This runbook is for **AI agent developers** integrating Context (via MCP) into daily coding workflows across many repos, many sessions, and mixed tool stacks.

The goal: keep context retrieval **fresh, bounded, and trustworthy**, without regressing into `rg/cat` loops.

## Golden path (daily workflow)

1) **Onboard once per repo / per session**
   - Use `repo_onboarding_pack` to get a compact map + a few key docs.
   - Then use `read_pack` (`intent=memory`) as the default “project memory” snapshot.

2) **Answer targeted questions**
   - Use `context_pack` when you need a single bounded payload that is easy to paste into the model context.
   - Use `search` for fast top-k code snippets; use `context` when you also want the graph halo.

3) **Read, don’t grep**
   - Use `read_pack` (`intent=file|grep|query`) instead of hand-rolling multiple tool calls.

## Root hygiene (avoid cross-project mixups)

Best practice: **always pass `path`** on tool calls when your client can.

When `path` is omitted, Context resolves the project root in this order:

1) Per-connection session root (from MCP `roots/list` or an explicit `root_set`)
2) `CONTEXT_ROOT` / `CONTEXT_PROJECT_ROOT`
3) (Non-daemon only) server process cwd fallback

Notes:

- Relative `path` values are resolved against the established session/workspace root (not the server process cwd).
- For `search` / `context` / `context_pack`, a **relative** `path` with an existing session root is treated as an in-repo scope hint (`include_paths` / `file_pattern`) instead of switching roots. Use `root_set` or an absolute `path` to switch projects.
- In shared daemon mode, a relative `path` without roots fails closed to avoid cross-project mixups.
- `root_get` / `root_set` are the explicit, multi-session-safe way to introspect or change the active session root (see `crates/mcp-server/src/tools/schemas/root.rs`).
- `root_get` includes `last_root_set` and `last_root_update` snapshots (with `source_tool` when available)
  to debug unexpected root drift.
- Agent-proof ergonomics: in an established session, some tools treat a **relative** `path` as an *in-project hint* (did-you-mean) instead of switching project roots (see schema descriptions: `crates/mcp-server/src/tools/schemas/map.rs`, `crates/mcp-server/src/tools/schemas/read_pack.rs`, `crates/mcp-server/src/tools/schemas/context_pack.rs`, `crates/mcp-server/src/tools/schemas/meaning_focus.rs`).

Every project-scoped tool response includes a `root_fingerprint` note in the `.context` output so clients can detect accidental cross-project context mixups without exposing absolute filesystem paths.

Error responses also include this note when a root is known, so provenance stays visible even when a call fails.
For root resolution failures, `details.root_context` provides a machine-readable snapshot
(`session_root`, `cwd`, `last_root_set`, `last_root_update`) to enable automated drift triage.

If a cursor continuation crosses roots, the `invalid_cursor` error includes details notes such as:
`details.expected_root_fingerprint` and `details.cursor_root_fingerprint`.

If a cursor continuation tries to change query-shaping parameters (e.g. `ls.file_pattern`,
`ls.allow_secrets`, `tree.depth`), Context **fails closed** with `cursor_mismatch` instead
of silently restarting pagination. The error includes actionable `next_actions` (see
[contracts/command/v1/error.schema.json](../contracts/command/v1/error.schema.json)).

## If semantic results look wrong

Treat this as a **trust failure**, not “the model is dumb”.

Recommended checks:

- If you expected an identifier (e.g. `LintWarning`) but results don’t mention it:
  - Prefer `text_search` (exact substring) to confirm the symbol exists in the repo.
  - If it’s missing, do not accept unrelated semantic hits as “best effort” context.
- If the index is stale (tool meta includes `stale=true`):
  - Semantic tools fail closed and suggest filesystem fallbacks while the index refreshes.
  - Retry later or increase `auto_index_budget_ms` (rarely needed after warmup).

## If `context_pack` returns 0 items

This is a valid outcome (and safer than “confident junk”).

Suggested recovery steps:

- Use `text_search` to validate the anchor token exists in the repo.
- Use `repo_onboarding_pack` to confirm the effective root and key docs.

## Regex hygiene (`rg`)

`rg` uses **Rust regex** by default.

Common pitfalls:

- If you intended a literal search, set `literal: true`.
- If you are calling via JSON, remember to escape backslashes:
  - to match a literal `(`, the regex is `\(`, and the JSON string is `\\(`.

If a regex is invalid, `rg` returns an `invalid_request` error with a hint (and may include compact `next:` suggestions).

## Freshness model (what to expect)

- Semantic tools never serve silently stale results.
- When the index is missing or stale, tools fall back to filesystem strategies and schedule refresh work in the background (when enabled).
- For deterministic CI and local dev without models:
  - Use `CONTEXT_EMBEDDING_MODE=stub`.

## Safety model (secrets)

- By default, filesystem tools skip common secret paths and refuse explicit reads of secret-looking files.
- Only opt in with `allow_secrets: true` when you genuinely need it, and keep outputs tightly bounded.

## Budgeting (keep outputs useful)

- Prefer `response_mode=facts` for daily use: low-noise, payload-first.
- Use `response_mode=full` for debugging (meta + diagnostics).
- Keep `max_chars` strict; increase only when you have a concrete reason.
