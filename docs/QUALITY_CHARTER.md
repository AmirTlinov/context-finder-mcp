# Quality Charter (Premium Daily Driver)

This document defines **non-negotiable invariants**, **measurable SLOs**, and **release gates**
that keep Context Finder reliable for **daily AI-agent use** across repos of any size.

This is intentionally contract-aligned:

- Anything crossing a process boundary is a **contract** (`contracts/**`, `proto/**`).
- Prose explains the intent and links back to contracts; it does not redefine them.

## 1) Product invariants (must hold)

### I1 — No cross-project contamination (fail-closed)

Context must never come from the wrong project root.

- Every response MUST include provenance in tool meta (at least `root_fingerprint` in MCP `full` mode).
- If the project root cannot be resolved unambiguously in shared/daemon mode, the tool must **fail closed**
  (return an error + next best action) rather than guessing.

### I2 — Boundedness and determinism

- All tools that can return large content MUST honor a hard budget (`max_chars` or equivalent).
- Truncation MUST be deterministic and cursor-continuable where applicable.
- Stub mode (`CONTEXT_FINDER_EMBEDDING_MODE=stub`) MUST be deterministic and used in CI.

### I3 — No silent correctness fallbacks

The system may fall back from semantic to filesystem strategies, but it MUST:

- make that decision explicit via meta/hints,
- provide the best bounded next action,
- never return “high-confidence junk” when anchors are missing.

### I4 — Secret-safe by default

- Read tools refuse common secret locations by default.
- `allow_secrets=true` is explicit opt-in and must be audited by tests.

## 2) Retrieval quality policy

### Q1 — Layered retrieval is mandatory

Semantic retrieval is never “alone”:

- If semantic is unavailable, fall back to bounded filesystem (`text_search` / `grep_context`).
- If semantic returns candidates but they are **anchorless**, fall back to filesystem or ask for clarification.

### Q2 — Anchors prevent “confident nonsense”

If a query contains a strong anchor (identifier / path / quoted phrase), the output MUST contain at least
one snippet that mentions that anchor (case-insensitive, whole-word when possible) OR the tool must
return a bounded fallback with an explicit “no anchor match” note.

### Q3 — Concepts are first-class

Not everything is a code symbol.

Tools like `explain`/`impact` should have a concept-safe path:

- If a symbol is missing from the graph, return best-effort evidence from docs/text when available.
- Never force the caller to “guess the right tool”: the tool should suggest the best next action.

## 3) Freshness policy (stale handling)

### F1 — Stale is a state, not an error

When the index is stale (e.g. `filesystem_changed`), the system MUST:

- either auto-reindex within a bounded budget, or
- degrade to filesystem strategies instead of returning silently stale semantic hits.

### F2 — Adaptive budgets by default

Default `auto_index_budget_ms` MUST scale with repo size and historical indexing p95, within a global cap.
The goal is: small repos feel instant; large repos remain reliable without manual babysitting.

## 4) Operability & transport policy

- MCP init MUST not block on client-provided capabilities that might only become available after init.
- The server MUST be resilient to client disconnects and pipe closes (no panics, no partial corrupt state).
- Shared backend MUST isolate per-session defaults (root/path/cursors) across concurrent sessions.

## 5) Metrics and SLOs (measured, gated)

We track these on a **repo matrix** (see Section 6):

### Core correctness

- **Wrong-root rate:** 0.0% (hard gate).
- **Anchorless-hit rate (for anchored queries):** ≤ 0.5% (target), hard gate at 1.0%.
- **Empty-but-should-hit rate:** ≤ 2% on the golden set (target), hard gate at 5%.

### Performance (best-effort gates)

- Median “cold query → first bounded answer”: ≤ 2s on small repos in stub mode.
- “Stale fix time” in auto mode: bounded (e.g., ≤ 15s cap for interactive tools).

### Output discipline

- “Budget overflow rate”: 0.0% (hard gate) — never exceed `max_chars`.
- Deterministic truncation: stable outputs for the same inputs in stub mode.

## 6) Repo matrix (coverage, not vibes)

Quality is validated across a representative matrix:

- Small single-crate Rust repo (fast feedback).
- Medium Rust workspace (multi-crate, tests, docs).
- TypeScript/JavaScript repo (node tooling, configs).
- Python repo (pyproject, venv ignores, notebooks optional).
- Monorepo (multiple languages + generated/vendor directories).
- “Docs-heavy” repo (many ADR/RFC/MD files, fewer symbols).
- “Large files” scenario (huge JSON/lockfiles to ensure bounded handling).

The repo matrix is implemented as a combination of:

- golden datasets under `datasets/**`,
- synthetic fixtures in tests (TempDir repos),
- and optional local “real repo” runs (not required for CI).

## 7) Release gates (must be green)

These are mandatory for any change that affects external behavior:

- `scripts/validate_contracts.sh`
- `scripts/validate_quality.sh` (convenience wrapper: gates + stub eval smoke)
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `CONTEXT_FINDER_EMBEDDING_MODE=stub cargo test --workspace`
- Eval smoke on golden datasets (see `context-finder eval` in `docs/QUICK_START.md`)

Breaking behavior changes require a new contract version line under `contracts/**/v(N+1)/`.
