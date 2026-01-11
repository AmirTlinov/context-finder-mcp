# Meaning Mode (v0): Implementation Plan

This document is a **flagship-level implementation plan** for a “meanings-first” context workflow:

- Agents receive **compressed, high-signal meaning** (structure + boundaries + relationships).
- Exact file text is fetched **only when needed**, via **evidence pointers**.

The goal is to reduce “99% noise for 1% signal” for agents, across repos of any size and language.

This plan is **contracts-first** and aligns with the non-negotiable invariants in:

- `docs/QUALITY_CHARTER.md` (especially I1/I2/I3)
- `docs/EVALUATION.md` (deterministic CI + golden datasets)

---

## 0) Executive summary

We add a new, deterministic output layer:

1) **Meaning Graph (MG)** — canonical, minimal facts about a repo:
   - boundaries (CLI/HTTP/env/config/files/DB/events)
   - entrypoints and “centers of gravity”
   - symbols and relationships (defines/imports/calls/reads/writes)
   - contracts (OpenAPI/AsyncAPI/JSON Schema/Proto) as first-class nodes

2) **Evidence Pointers (EV)** — compact references to exact source material:
   - always includes provenance (`source_hash`, file path, span)
   - supports stable anchors (byte spans) when available

3) **Cognitive Pack (CP)** — agent-facing, **ultra-compact** representation of MG+EV:
   - uses a local dictionary and IDs to minimize repetition
   - designed to be cheap in LLM tokens, and machine-parsable

The system responds using a strict “semantic zoom” policy:

`Map (meaning)` → `Focus (meaning)` → `Evidence (verbatim)` → (optional) `Narrative`

This is a *change in what we transmit*, not text compression.

---

## 1) Goals / non-goals

### Goals (v0)

- **10–50× token reduction** for onboarding and locate-style workflows compared to verbatim reads.
- **Evidence-backed correctness**:
  - any non-trivial claim must have ≥1 evidence pointer.
  - evidence pointers are invalidated by file changes (`source_hash` mismatch).
- **Deterministic and bounded**:
  - honors hard budgets
  - deterministic truncation
  - cursor-continuable where applicable
- **Multi-session safe**:
  - never returns meaning/evidence from the wrong root
  - fail-closed when root cannot be resolved unambiguously
- **Language-agnostic**:
  - supports multiple languages via adapters
  - v0 focuses on “structural meaning”, not deep semantics

### Non-goals (v0)

- Perfect semantic understanding of dynamic languages, macros, or code generation.
- “Business meaning” explanation without evidence.
- Replacing existing `context_pack` / `read_pack` tools (we keep them).
- Using diagrams as a canonical source of truth.

---

## 2) Definitions and invariants

### Definitions

- **Meaning**: compact, checkable facts that allow correct navigation and next-step selection.
- **Evidence**: exact source fragments sufficient to verify a meaning claim.
- **Narrative**: human-friendly explanation derived from meaning, never introducing new facts.

### Product invariants (must hold)

1) **No cross-project contamination** (fail-closed), per `docs/QUALITY_CHARTER.md`.
2) **Boundedness and determinism**:
   - the tool is not allowed to “solve quality by flooding”
3) **No silent correctness fallbacks**:
   - if confidence is low, we must say so and provide a best next action

---

## 3) External surfaces (contracts-first)

We keep existing behavior intact and add a new surface.

### 3.1) Command API (canonical JSON schema)

Add new `action` values to `contracts/command/v1/command_request.schema.json`:

- `meaning_pack` — return a CP document + structured metadata.
- (optional) `meaning_focus` — focus around a node (can also be `meaning_pack` with mode).
- `evidence_fetch` — fetch exact text for one or more evidence pointers.

Add canonical output schemas:

- `contracts/command/v1/meaning_pack.schema.json`
- `contracts/command/v1/evidence_fetch.schema.json`
- (optional) `contracts/command/v1/meaning_graph.schema.json` (internal canonical model)

Guideline: CP is the default agent-facing payload; MG JSON is for debugging/automation and tests.

### 3.2) MCP tools (agent ergonomics)

Add MCP tools (names TBD; keep them short and descriptive):

- `meaning_pack` — main entrypoint (budgeted, deterministic).
- `meaning_focus` — semantic zoom-in (node/edge centric).
- `evidence_fetch` — exact span fetch (verbatim, minimal window).
- `diagram_handle` (optional v1) — return a handle, not the SVG itself.

Tools must follow repository standards:

- schemas in `crates/mcp-server/src/tools/schemas/*`
- wired via `crates/mcp-server/src/tools/dispatch/`

### 3.3) Provenance meta (anti-contamination)

Responses must include (at least in debug/full mode):

- `root_fingerprint` (already exists in `CommandResponse.meta`)
- `graph_version` (monotonic per root, increments on index updates)
- per-evidence `source_hash`

---

## 4) Canonical data model

### 4.1) Meaning Graph (MG) — minimal v0

MG is a graph of nodes and edges with evidence pointers.

#### Node types (v0)

- `project` / `workspace`
- `package` / `crate` / `module`
- `file`
- `symbol` (fn/type/class/trait/interface/const)
- `boundary` (cli/http/env/config/file_io/db/event)
- `contract` (openapi/jsonschema/proto)

#### Edge types (v0)

- `contains`, `defines`, `exports`, `imports`
- `calls`, `references`, `implements`
- `reads`, `writes`
- `uses_contract`, `owns_contract`

#### Node/edge fields (v0)

- `stable_id` (deterministic)
- `label` (short)
- `facets` (type/signature/visibility/async/errors when available)
- `confidence` (1.0 for deterministic extraction, <1.0 for heuristic)
- `evidence[]` (≥1 for any non-trivial fact)

### 4.2) Evidence Pointers (EV)

EV must be minimal, stable, and verifiable.

EV v0 (pragmatic):

- `file` (repo-relative path)
- `start_line`, `end_line` (bounded window)
- optional: `byte_start`, `byte_end` (stable anchor when available via AST)
- `source_hash` (hash of file bytes; invalidates EV on change)

### 4.3) Cognitive Pack (CP) — token-efficient encoding

CP is a compact text representation of MG+EV designed to:

- minimize repeated strings via a dictionary
- allow agents to reference IDs, not full paths/names
- be deterministic and machine-parsable

CP v0 structure:

- `D` lines: dictionary entries (paths/symbol names) → `d0`, `d1`, ...
- `N` lines: nodes: `n7 kind dName dFile [facets...] [ev:ev3,ev8]`
- `E` lines: edges: `n7 rel n2 [ev:ev3]`
- `EV` lines: evidence: `ev3 dFile L10 L25 hash=...`
- `NBA` line: a single “next best action”

Note: CP is the agent default; JSON MG is for tests/debug.

---

## 5) Retrieval policy (“semantic zoom”)

This policy is not optional; it is the core product.

### 5.1) Intent classification (deterministic v0)

Classify each request into one of:

- `onboarding` — “what is this system and where is the entrypoint?”
- `locate` — “where is X configured/implemented?”
- `trace` — “how does A reach B?”
- `impact` — “what changes if I touch Y?”
- `contract` — “what is the IO format?”

If intent is unclear:

- return a minimal `Map` CP
- ask at most one clarifying question (or provide NBA to narrow)

### 5.2) Ranking (graph-first)

Rank candidates using stable signals:

- graph proximity to boundaries/contracts
- entrypoint-centrality heuristics
- symbol/path exact matches
- confidence
- freshness (recently changed files)

Embeddings can be used only as a ranking signal for:

- names, docstrings, READMEs, contract descriptions

Embeddings must never create facts.

### 5.3) Output shaping (hard budgets)

Never output “the whole module”.

Priority for `Map` output:

`Contracts/Boundaries` → `Entrypoints` → `Top edges` → `Pointers/NBA`

Stop as soon as the user can take the next correct step.

---

## 6) Architecture integration (use existing patterns)

We prefer integrating into existing components rather than inventing new layers.

### 6.1) Indexing pipeline

Context Finder already builds:

- a corpus (chunks + metadata)
- a fuzzy index
- an embeddings index (optional)
- a code graph (relationships)

Meaning Graph should be:

- derived from existing symbol extraction + relationship graph
- enriched with contract/boundary extraction

### 6.2) Shards and incremental updates

Compute meaning as per-file shards:

- `1 file → 1 meaning shard`
- shard is keyed by `source_hash`
- on change: recompute shard, then merge

This enables O(changed_files) updates and avoids full rescans.

### 6.3) Storage

Use the existing on-disk index location (`.agents/mcp/context/.context/`), add:

- a meaning shard store (by file + source_hash)
- indices (symbol → node ids, path → file ids)
- a graph version counter

Implementation detail (v0 preference): keep it simple and deterministic (e.g., sqlite WAL or existing storage pattern).

---

## 7) Multi-session safety model

We must treat this as a security boundary.

### 7.1) Root resolution

- Root is resolved only from MCP roots or explicit request `path`.
- If root is missing/ambiguous in daemon mode: **fail closed** and return NBA.

### 7.2) Path safety

For any path read (including EV):

- normalize + `realpath`
- enforce `realpath(target)` is within `realpath(root)`
- deny symlink escapes

### 7.3) Shared cache vs per-session state

Safe to share (per root_fingerprint):

- meaning shards
- indices
- embeddings cache

Must be per-session:

- CP dictionary IDs (unless explicitly negotiated)
- working set / focus context

---

## 8) Diagrams (optional v1, not v0)

Diagrams are a **view**, not truth.

### 8.1) Deterministic SVG

- stable node ordering (by stable_id)
- stable layout seed
- coordinate quantization to reduce jitter

### 8.2) Token policy

- the default response returns a `diagram_handle`, not SVG
- SVG is fetched only when a UI requests it

---

## 9) Evaluation and release gates

We will not ship this based on vibes.

### 9.1) CI layer (mandatory)

- deterministic mode (`CONTEXT_FINDER_EMBEDDING_MODE=stub`)
- tests for:
  - no cross-root contamination
  - boundedness
  - determinism of CP outputs
  - “no claim without evidence” invariants
  - path safety (symlink escape)

### 9.2) Golden datasets (regression-proof)

Add a small dataset for meaning workflows:

- queries that should produce correct boundaries/entrypoints
- expected paths/symbols in top-k
- expected evidence coverage for claims

### 9.3) Metrics gates (v0)

On onboarding/locate tasks:

- `token_saved >= 0.80`
- `evidence_coverage >= 0.95`
- wrong-root rate must be `0`

If gates fail: the feature remains disabled by default.

---

## 10) Phased delivery plan

### Phase 0 — Contracts + scaffolding (1–2 days)

Deliverables:

- schemas for MG/EV/CP (v1 line)
- MCP tool schemas + dispatch wiring
- minimal CLI command plumbing
- tests for schema validation

Exit criteria:

- contracts validation passes
- tools compile, stub mode tests green

### Phase 1 — Meaning v0 (structure + boundaries) (3–7 days)

Deliverables:

- meaning extraction for:
  - workspace/package layout
  - entrypoints
  - contracts
  - boundaries (CLI/HTTP/env/config)
- evidence pointers for each extracted fact
- CP output with strict budgets + NBA

Exit criteria:

- CI tests + small meaning dataset pass
- manual run on 3 repos shows ≥80% token_saved

### Phase 2 — Meaning v0+ (relations + trace/impact) (1–2 weeks)

Deliverables:

- call/uses relations surfaced in CP
- `trace` intent (subgraph from A to B)
- `impact` intent (reverse edges, owners)
- working set support (semantic zoom)

Exit criteria:

- improved recall@k without output bloat
- stable performance on larger repos

### Phase 3 — Optional narrator + diagrams (gated) (later)

Deliverables:

- narrator that is forced to cite ev_ids
- diagram handle + deterministic SVG renderer

Exit criteria:

- narrator passes “no new facts / must cite evidence” validators
- diagrams do not affect default token budgets

---

## 11) Open questions (explicitly tracked)

1) CP format finalization:
   - do we standardize a single grammar (recommended) or allow variants?
2) Evidence anchoring strategy:
   - byte spans available for which languages first?
3) Storage choice:
   - reuse existing index store implementation vs introduce sqlite store
4) Contract/Boundary extraction depth:
   - how far do we go without turning v0 into “semantic everything”?

Answer these by running Phase 0 + a minimal Phase 1 eval; do not bikeshed upfront.
