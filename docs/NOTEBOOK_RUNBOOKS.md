# Notebook + Runbooks (Agent Memory Without Noise)

Context Finder’s meaning tools help agents navigate a repo quickly, but real work is rarely a single session.
This feature set makes context **durable**, **evidence-backed**, and **refreshable** across `/compact`,
new sessions, and multiple agents.

## The mental model

- A **notebook** is a repo-scoped set of agent-authored anchors (“hot spots”) + runbooks.
- A **runbook** is a curated, deterministic “lens” that refreshes only the subsystem context you care about.
- A runbook is **not** a search loop: it produces bounded outputs with explicit freshness/staleness truthfulness.

## Tools

- `notebook_pack`: read-only snapshot of saved anchors/runbooks (can mark evidence as stale).
- `notebook_edit`: explicit writes (upsert/delete) with locking + atomic updates.
- `notebook_suggest`: read-only generator that proposes a ready-to-apply starter set
  (anchors + runbooks) derived from evidence-backed `meaning_pack`.
- `runbook_pack`: refresh runner
  - default mode is **TOC** (low-noise)
  - expand exactly one section when needed
  - long sections paginate via a **cursor** continuation (bounded by `max_chars`)

## Design guarantees (product invariants)

- **Trust-by-design**: anything non-trivial is anchored to evidence pointers (file + line span + optional hash).
- **Fail-closed on uncertainty**: when evidence invalidates, the output marks it stale instead of “hallucinating”.
- **Deterministic + budgeted**: outputs are bounded and stable under the same inputs; no unbounded scans.

## Recommended usage patterns

1) Create 3–8 anchors for entrypoints/contracts/CI + the “core boundary”.
2) Create 1–3 runbooks:
   - a “daily portal” (what to run/verify + what changed)
   - a “subsystem lens” (contracts + CI + the core directory)
   - a “worktrees lens” (active worktrees/branches when `.worktrees/` is used)
3) In daily use: `runbook_pack` (TOC → expand a single section).
