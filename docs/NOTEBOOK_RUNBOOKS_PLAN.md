# Context Notebook + Runbooks (v1): Implementation Plan

This document is a **flagship-level implementation plan** for an agent-native feature set:

- Agents can **persist their own semantic anchors** ("hot spots") while exploring a repo.
- Agents can define **runbooks** that re-fetch *only what they care about* with **freshness + staleness truthfulness**.
- After `/compact` or a new agent/session, the system can instantly restore **high-signal context** with **low noise**.

This plan is **contracts-first**, evidence-first, and aligned with the non-negotiable invariants in:

- `docs/QUALITY_CHARTER.md`
- `docs/EVALUATION.md`

---

## 0) Executive summary

We add a durable, repo-scoped knowledge layer (the "notebook") and a deterministic refresh engine (the "runbook runner").

Key idea: **a runbook is a curated lens**, not a search. It is anchored in **EvidencePointers** (file + line span + optional `source_hash`) and yields **freshness-aware outputs**:

- If the evidence still matches: show it (bounded).
- If it changed: **do not pretend**; mark stale and provide bounded re-anchor guidance (fail-closed).

---

## 1) Product promise (where we are objectively better)

### Scenario A — Cross-session continuity ("hot spots survive /compact")
An agent explores a repo once, saves anchors for the true entrypoints/contracts/gates, and later (or in another session/agent) restores the same high-signal context in **one call**.

### Scenario B — Subsystem refresh ("give me updates without noise")
An agent defines a runbook for a subsystem (e.g., API contracts + CI gates + core boundary). Running the runbook returns:
fresh evidence excerpts + status summary + next actions, with **no directory listing spam**.

### Scenario C — Handoff safety ("another agent can pick up")
A team shares a runbook (optional repo-committed flavor). Any agent can run it and immediately see:
what is true, what became stale, and what needs verification.

---

## 2) Invariants (trust-by-design)

These are **hard product invariants**:

1) **Evidence-backed**: Any non-trivial claim must be grounded in evidence pointers.
2) **Fail-closed freshness**: If evidence cannot be verified, it is not asserted as true.
3) **Deterministic + bounded**: Stable ordering, stable truncation rules, strict `max_chars` budgets.
4) **No secrets**: Secret paths remain blocked for both reading and anchoring by default.
5) **No cross-root leakage**: Notebook entries are scoped and validated by repo identity.

---

## 3) Concepts & data model

### 3.1 Notebook
A notebook is a repo-scoped store containing:

- `anchors[]`: semantic hot spots
- `runbooks[]`: refresh recipes referencing anchors and/or existing tools
- minimal `meta`: schema version, repo identity, timestamps

### 3.2 Anchor
An anchor is a durable reference to something meaningful:

- `id` (stable)
- `kind` (canon/ci/contract/entrypoint/zone/other)
- `label` (human-friendly)
- `evidence[]`: list of EvidencePointers (file + line range + optional `source_hash`)
- `locator` (optional): a deterministic re-anchor hint (symbol name, regex, snippet hash)
- `freshness`: captured_at, optional git head, optional index watermark

### 3.3 Runbook
A runbook is a deterministic refresh lens:

- `id`, `title`, `purpose`
- `inputs`: optional parameters (worktree/branch focus, budgets)
- `steps`: a compact graph of tool calls (reuses Batch v2 semantics)
- `outputs`: which parts to surface (anchors, evidence excerpts, CI gates, etc.)
- `policy`: budgets + freshness policy + strictness + noise budget

---

## 4) Repo identity & scoping

Problem: worktrees and clones share "the same repo" but have different filesystem roots.

Design:

- Compute `repo_id` from the git **common dir** when available (unifies worktrees).
- Also retain `root_fingerprint` for safety + quick mismatch detection.
- Notebook storage is keyed by `repo_id`, while runbook execution is parameterized by a chosen `root`.

If `repo_id` cannot be determined, fall back to `root_fingerprint` (still safe, less shareable).

---

## 5) Storage layout & durability

### 5.1 Default storage (private, durable)
Store notebooks under the project-scoped agent cache directory (preferred layout).

Requirements:

- Atomic writes (temp + rename)
- File locking to support multi-agent access
- Size budgeting (caps for cached excerpts / number of anchors)
- Garbage collection for superseded snapshots

Kill switch:

- A single env/config switch must be able to disable notebook I/O entirely (read + write) for safety.

### 5.2 Optional export (shareable)
Allow exporting runbooks (and optionally anchors) into a repo-visible location for team handoff.

Exports must be:

- schema-versioned
- independent of local absolute paths
- safe to run (no secrets)

---

## 6) Tool surface (agent UX)

Goal: keep the public surface narrow while enabling strong workflows.

### 6.1 Read + run (1-call)
- `runbook_pack`: run a named runbook and return a compact, evidence-backed pack with freshness statuses.
- `notebook_pack`: show notebook index (anchors/runbooks) with low-noise statuses and suggested next actions.

### 6.2 Writes (explicit)
- `notebook_edit`: upsert/remove anchors and runbooks (explicit, no implicit writes).

All outputs support `response_mode` and strict budgeting.

### 6.3 Integrations (no extra calls)

- `read_pack intent=memory` should optionally surface notebook anchors as a **first-class memory source**
  (bounded, low-noise, evidence-pointer only by default).
- `meaning_pack` / `atlas_pack` can emit a single `next_action` (Full mode only) to “save suggested anchors”
  without spamming per-anchor actions.

---

## 7) Runbook execution engine

We reuse the existing Batch v2 `$ref` semantics and add a thin execution layer:

- Resolve runbook → internal step graph → execute in-process tool calls.
- Apply a deterministic "output selector" to keep only high-signal fields.
- Produce a compact, stable encoding (similar to CPV1/WTV2 style).

---

## 8) Freshness & re-anchoring

### 8.1 Status model
Every anchor/evidence item returns a status:

- `fresh` (hash matches, file exists, span valid)
- `stale_hash_mismatch`
- `missing_file`
- `span_invalid`
- `blocked_secret`
- `unknown` (no hash provided; treated as lower confidence)

### 8.2 Re-anchor pipeline (deterministic)
When stale, attempt bounded re-anchoring (in order):

1) same file + snippet hash match window
2) symbol-based re-anchoring (when symbol known)
3) rg (legacy: grep_context) constrained to the anchor’s zone scope

If re-anchor succeeds, emit new evidence pointers **and mark the move** (with evidence).
If it fails, emit next actions to guide the agent (fail-closed).

---

## 9) Evaluation (“zoo”) and product metrics

Extend eval-zoo to cover:

- research repos
- dataset-heavy repos
- polyglot repos
- repos with no docs
- repos with heavy `.worktrees` usage

Core metrics (tracked per archetype):

- `calls_to_action` (<= 2 for onboarding & refresh)
- `token_saved` (mean and tail)
- `noise_ratio`
- `latency_ms` (p50/p95/max)
- `stability` (same input → same output)
- `staleness_rate` (how often anchors go stale)
- `repair_rate` (how often deterministic re-anchor succeeds)

---

## 10) Delivery & release train

### 10.1 Contracts-first workflow
Before implementation:

- Add JSON Schemas for notebook + runbook requests/results.
- Add/extend command action enums if exposed via CLI/HTTP.
- Add MCP tool schemas and wire them through dispatch.

### 10.2 Versioning
All notebook/runbook payloads are schema-versioned.
Runbook runner is backwards-compatible within a major series; breaking changes bump the contract line.

### 10.3 Changelog
Every behavioral change to runbook outputs requires a changelog entry + eval delta.

---

## 11) Roadmap (phased, product-grade)

Phase 1 — Contracts + core model + storage (locking/atomic/GC)

Phase 2 — Tool surface (pack/edit/run) + deterministic encodings + staleness statuses

Phase 3 — Re-anchor pipeline + strictness policies + UX next-actions

Phase 4 — Eval-zoo expansion + CI gates for regressions + stability/latency/noise dashboards

Phase 5 — Shareable exports + team handoff flows + release polish (install/update compatibility)

Definition of Done:

- Golden scenarios A/B/C are repeatably achievable in <=2 external calls.
- Strict gates green; eval-zoo shows no regressions; notebook/runbook outputs remain deterministic.

---

## 12) Risk register (top) + mitigations

- **Mis-anchoring after edits** → bounded re-anchor pipeline + explicit “moved” markers + fail-closed default.
- **Noise creep** → noise budgets per surface + eval gates for noise_ratio + “minimal claims when no evidence”.
- **Concurrency corruption** → file locking + atomic writes + schema versioning + integrity checks.
- **Secret leakage** → reuse secret filters; disallow anchoring to blocked paths by default.
