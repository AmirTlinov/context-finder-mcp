# Meaning Mode (v1): Repo Lenses + Anchor Graph

This document is a **flagship-level** implementation plan to make `meaning_pack` / `meaning_focus`
work **predictably well** on *extreme / atypical repositories* (artifact-heavy, research-first,
codegen-heavy, polyglot, bespoke build systems), while preserving the non-negotiable invariants:

- **Bounded + deterministic output**
- **No claim without evidence**
- **Multi-session safe (no cross-root contamination; fail-closed)**
- **Additive only** (must not degrade existing `read_pack` / `context_pack`)

This plan **extends** (does not replace) `docs/MEANING_MODE_PLAN.md`.

---

## 0.1 Current status (already shipped in code)

The following “v1 primitives” are already implemented (without changing public tool inputs):

- **`S ANCHORS` in CPV1**: `meaning_pack` emits a bounded list of `ANCHOR ... ev=...` lines first.
  Anchors are **evidence-backed** and treated as “claims” (no EV → no anchor).
- **Semantic zoom bias**: evidence candidates are ordered so `NBA` points to a high-signal anchor
  (canon/how-to) instead of an arbitrary boundary.
- **Optional SVG diagram output**: `meaning_pack` / `meaning_focus` can return an `image/svg+xml`
  “Meaning Graph” derived from CP (no extra claims beyond evidence-backed facts).
- **k8s manifest hardening (fail-soft)**: a bounded, deterministic pass can surface
  “k8s-like manifests” even outside conventional `k8s/`/`manifests/` directories.
- **Infra anchor coverage**: infra boundaries/anchors now treat `k8s`/`helm`/`terraform` as first-class,
  and include common GitOps layouts (Flux/ArgoCD) as evidence candidates.
- **Artifact store anti-noise**: `meaning_pack` suppresses common artifact store scopes
  (`artifacts/`, `results/`, `runs/`, `outputs/`, `checkpoints/`) from `S MAP` so structure stays useful,
  and emits an evidence-backed `ANCHOR kind=artifact` when a suitable artifact doc/manifest exists.

This is the foundation for the full lens pipeline described below.

## 0) Problem statement (why v0 degrades on “weird” repos)

On repositories where:

- the majority of files are **artifacts/results** (often 10×–100× the code),
- the “truth layer” lives in **profiles**, **experiments**, **Make/CMake**, **docs canon**, and
  **artifact contracts** (not in language entrypoints),

…a naive “map first” approach degenerates into:

- huge directory counts (`artifacts/` dominates),
- shallow structural hints,
- little/no **actionable anchors** and little/no **evidence-backed navigation**.

Meaning mode must become a **sense-first compass**:

> “What is the canon? Where is truth? How do I run/compare? What are outputs?
> What is the minimum next correct step?”

---

## 1) Definition of “orientation by sense” (DoD)

### 1.1 Output DoD (what the agent gets)

Given any repo root, `meaning_pack` must, within `max_chars≈2000–4000`:

1) Emit **3–7 anchors** (actionable “where to start / canon / run / outputs / contracts”), each with:
   - `label` (human-usable)
   - `kind` (canon|howto|entrypoint|contract|artifact|experiment|infra)
   - ≥1 evidence pointer (`EV`)
2) Emit **one canonical pipeline sketch** (a small sequence of steps),
   **only** from evidence-backed sources (docs/profiles/build/CI).
3) Emit **boundaries** that matter for navigation:
   - run/test/serve (even if non-standard)
   - config/profile system
   - artifacts as outputs (without enumerating thousands of files)
4) Emit **NBA** (next best action) that points to a single evidence fetch or a focus step.

If it cannot do (1) with high confidence, it must **fail-soft**:

- return minimal map + **one** clarifying question OR a single NBA to fetch a likely canon doc,
- never flood content to “solve quality by volume”.

### 1.2 Product invariants (hard)

- **No cross-root**: all claims/evidence must belong to resolved root fingerprint, or the tool fails.
- **Multi-session safe**:
  - cursor aliases must not collide across concurrent MCP processes (persistence must be lock-protected),
  - failures degrade to “expired continuation” (never to wrong-root expansion),
  - shared backends must be **fail-closed** on missing/ambiguous root (no cwd-guessing in daemon mode).
- **No claim without EV**:
  - “Non-trivial claim” = anything that can mislead navigation (entrypoint, “how to run”, “canon”).
  - If EV cannot be attached, claim is either dropped or marked as hypothesis with low confidence
    (and must not be used for NBA).
- **Determinism**:
  - stable extraction and stable ordering for the same repo state.
- **Boundedness**:
  - tool respects budgets and performs deterministic truncation.

---

## 2) Core idea: “Repo Lenses” (deterministic meaning extractors)

Meaning v1 is implemented as a **lens pipeline**:

1) **Archetype detector** (cheap, deterministic)
2) **Lens selection** (deterministic)
3) **Lens execution** → produces *claims + evidence pointers*
4) **Anchor Graph assembly** → collapses lens outputs into a compact meaning graph
5) **CP serializer** → budgeted, stable “sense-first” output

### 2.1 Why lenses (architectural elegance)

Lenses let us:

- encode “sense” without fragile NLP guesses,
- stay deterministic,
- remain language-agnostic,
- keep the core small (a few lenses) while allowing incremental evolution.

The key is: **lenses emit claims**, not prose.

---

## 3) Data model: Claim + Evidence (minimal, composable)

### 3.1 Claim (internal canonical)

Each lens returns `Claim` items:

- `claim_id` (stable; deterministic hash of `kind + key + evidence`)
- `kind` (anchor|boundary|canon_step|contract|artifact_store|entrypoint|glossary_term)
- `label` (short)
- `confidence` (0..1, deterministic sources=1.0, heuristic sources<1.0)
- `evidence[]` (>=1 for non-trivial)
- optional `links[]` to other claims (for canon pipeline / boundaries)

### 3.2 Evidence Pointer (EV)

Use the existing EV format (repo-relative file + line span + `source_hash`).

Rules:

- EV is always verifiable with `evidence_fetch` and can be strict via hash.
- EV spans are **small** (bounded), stable, and deterministic.

---

## 4) Archetype detector (cheap, deterministic)

The detector classifies a repo into 1–2 dominant archetypes using stable signals:

- Directory mass distribution (top directories by file count)
- Presence/absence of canonical files:
  - docs canon (`PHILOSOPHY`, `GOALS`, `docs/README`)
  - build systems (`Makefile`, `CMakeLists`, `Bazel`, etc.)
  - experiments/profiles (`profiles/`, `examples/profiles`, `configs/`)
  - artifacts/results (`artifacts/`, `runs/`, `results.json`, `*.ckpt`, `*.log`)
  - CI (`.github/workflows/*`)
- Language mix (extensions distribution)

Example: if a single directory contains >60% of files and matches artifact patterns,
flag archetype `artifact_heavy=true`.

The archetype is used **only** for ranking and lens selection, never as a claim.

---

## 5) Lens catalog (v1 minimal set)

### 5.1 Canon Lens (highest priority)

Goal: find *the one reality* / “where to start” / philosophy/goals/architecture.

Inputs:

- README(s), docs index files, “canon” docs (PHILOSOPHY/GOALS), AGENTS.

Output:

- `anchor(kind=canon)` to the best starting doc(s)
- `canon_step` chain if the doc contains a pipeline-like sequence (deterministic heuristics:
  headings like “Canon”, arrows, numbered lists, “→”, etc.)

Evidence extraction:

- small spans around the relevant headings/bullets.

### 5.2 How-to-Run Lens (build/run/test boundaries)

Goal: “what command do I run, what environment knobs exist”.

Inputs:

- Makefiles, CMake, scripts, CI workflows, common runner scripts.

Output:

- `boundary(kind=build|run|test|serve)` claims with EV
- `anchor(kind=howto)` (e.g., “build entry”, “run entry”, “evaluate entry”)

Important: this lens must support non-production repos (research pipelines, eval suites).

### 5.3 Infra Lens (deploy boundaries)

Goal: identify “how this ships” and the deploy surface (k8s/helm/terraform + GitOps) without
enumerating thousands of manifests.

Inputs:

- `k8s/`, `kubernetes/`, `manifests/`, `deploy/`, `infra/`
- `charts/`, `helm/`, `helmfile.*`, `HelmRelease`
- `*.tf`, `*.tfvars`, `*.hcl`, `terragrunt.hcl`
- GitOps layouts: `flux/`, `gitops/`, `argocd/`, `Application`/`ApplicationSet`, `clusters/`

Output:

- `anchor(kind=infra)` (“Infra: deploy”) pointing to a single highest-signal infra file
- optional `boundary(kind=deploy)`-style claims (internally) that feed the anchor graph

Evidence:

- cite the specific manifest/chart/module file that establishes the deploy boundary.

### 5.4 Contracts Lens (IO truth)

Goal: surface contracts even if they’re not OpenAPI/proto (research often has “artifact contract” docs).

Inputs:

- explicit contract directories (`docs/contracts`, `contracts`, `proto`)
- file patterns (`*_contract*`, `schema`, `spec`, `protocol`)

Output:

- `contract` anchors + boundary links (“this output is produced by this pipeline”)

### 5.5 Artifact Store Lens (anti-noise)

Goal: handle enormous outputs without drowning the map.

Inputs:

- directories matching artifact patterns (artifacts/results/runs/cache)

Output:

- `artifact_store` claim:
  - *what kinds of outputs* (results, checkpoints, logs)
  - *how to locate a single exemplar* (rules / index files)
  - *do not enumerate files*

Evidence:

- if there is a contract doc describing artifacts: cite it
- else cite a small directory README / manifest / naming convention snippet
- else degrade to a “store exists” claim with low confidence and minimal NBA (“fetch artifacts README”)

### 5.6 Code Skeleton Lens (only after canon/howto)

Goal: give a code “map” without reading code.

Inputs:

- small set of likely core dirs (`src`, `crates`, `lib`, `runtime`, `internal`)
- “entrypoint candidate” patterns per language (but deterministic)

Output:

- top modules (not per-file list)
- entrypoint candidates (only if EV exists)

This lens must not dominate artifact-heavy repos.

---

## 6) Anchor Graph (meaning graph that matches how humans think)

From claims, build a small graph with fixed node kinds:

- `StartHere` (canon doc)
- `Canon` (pipeline steps)
- `HowToRun` (build/run/test)
- `Outputs` (artifact stores + contracts)
- `Interfaces` (contracts + protocol files)
- `Core` (code skeleton: runtime/internal/core)

Edges are deterministic (derived from claim kinds), and all nodes must have EV except:

- synthetic grouping nodes (`Core`, `Outputs`) which must have EV on their children.

This graph is **the** “sense layer”.

---

## 7) CP output shaping (noise-free, budgeted)

We keep CP as line-based, forward-compatible (unknown sections are ignored by parsers).

### 7.1 CP section priority (always this order)

1) `S ANCHORS` (3–7 anchors max)
2) `S CANON` (pipeline sketch, max ~8 lines)
3) `S BOUNDARIES` (run/test/build/serve; max ~8 lines)
4) `S OUTPUTS` (artifact stores + contracts; max ~8 lines)
5) `S MAP` (small structural map; never dominated by artifacts)
6) `S EVIDENCE` (minimal EV set; prefer EV referenced by anchors/NBA)
7) `NBA` (always present)

### 7.2 Deterministic truncation

- If `max_chars` is tight, drop sections from the tail, but never drop:
  - `S ANCHORS`, `S CANON` (if present), and `NBA`.
- If evidence budget is tight, keep evidence for anchors first.

---

## 8) Engineering integration (minimal surface; maximum leverage)

### 8.1 Where code lives

- Implement lens logic in one place (avoid spreading heuristics):
  - `crates/cli/src/command/services/meaning.rs` (core meaning engine)
  - shared helpers (evidence extraction, file classification) in `meaning_common`

### 8.2 Do not add new external knobs by default

We should be able to ship v1 improvements **without** changing public schemas:

- CP is a string → adding sections does not break contracts.
- Keep tool inputs unchanged; lens selection is internal.

If we need debug/inspection, add it only in:

- `response_mode=full` (and only if contract allows it),
- or as a new versioned contract line (`v2`) if needed later.

---

## 9) Evaluation: make “sense quality” measurable

### 9.1 New metrics (meaning-specific)

For each repo + query:

- `time_to_first_anchor`: number of tool calls until an anchor EV points to the correct canon/how-to doc.
- `evidence_coverage`: fraction of non-trivial claims with ≥1 EV (target: ≥0.95).
- `artifact_noise_ratio`: fraction of output characters spent on artifact listings (target: near 0).
- `token_saved`: compare CP vs verbatim onboarding reads (target: ≥0.8 saved on onboarding).
- `wrong_root_rate`: must be `0`.

### 9.2 Golden datasets (stress)

Add at least one “extreme” golden case:

- artifact-heavy research repo (Pinocchio-class)
- codegen-heavy repo (generated code dominates)
- polyglot monorepo (many languages + tools)

### 9.3 Gates

- determinism: repeated runs are identical for same repo state
- boundedness: never exceeds `max_chars`
- no-claim-without-evidence (except synthetic group nodes)
- wrong-root=0

---

## 10) Phased delivery (fast proof, then deepen)

### Phase 0 — Baseline + failure characterization (0.5–1 day)

- Add a “weird repo” smoke harness that records:
  - current `meaning_pack` output
  - anchor availability and EV coverage
- Define baseline metrics.

Exit: baseline captured; failure modes enumerated.

### Phase 1 — Canon + How-to-Run + Infra lenses (1–3 days)

- Implement Canon Lens (anchors + canon pipeline)
- Implement How-to-Run Lens (build/run/test from Make/CMake/CI/scripts)
- Implement Infra Lens (deploy boundary from k8s/helm/terraform + GitOps)
- Output shaping: always show anchors first; always include NBA.

Exit: on Pinocchio-class repo, `3–7 anchors with EV` within 4000 chars.

### Phase 2 — Artifact Store lens (2–4 days)

- Detect artifact stores and summarize outputs without enumeration
- Link outputs to contracts/docs when available

Exit: artifact noise ratio near zero; outputs navigable via EV.

### Phase 3 — Code skeleton + directory focus upgrade (3–7 days)

- For `meaning_focus` on directories: top modules + “core files” shortlist with EV
- Optional lightweight outline for a few core files (budgeted, deterministic)

Exit: directory focus yields a usable next step, not just counts.

### Phase 4 — Hardening + expansion (ongoing)

- add archetype refinements
- add polyglot rules
- expand golden set

---

## 11) Risks & fastest disproof tests

### Risk: “anchors are wrong / misleading”

Test:

- enforce that anchors must cite a canon heading/section (EV-based)
- if not found, degrade to a question/NBA.

### Risk: “becomes non-deterministic”

Test:

- run identical `meaning_pack` N times; byte-for-byte output must match.

### Risk: “performance regressions on huge repos”

Test:

- enforce per-lens time budgets and per-lens file caps (deterministic selection).

### Risk: “cross-process cursor aliases collide (wrong continuation / wrong root)”

Test:

- run two MCP server processes concurrently and force many cursor allocations; ensure no collisions,
  and that any persistence failure degrades to “expired continuation” (never wrong expansion).

### Risk: “accidentally enumerates artifact trees”

Test:

- golden repo with huge artifacts; ensure output has zero raw file listings from that directory.

### Risk: “OS watch limits make freshness non-reactive”

Test:

- force a watcher backend with `0` active watches (e.g., exhausted inotify) and verify a polling
  fallback keeps updates flowing under bounded CPU.

---

## 12) Recommended next action

Start with **Phase 1** (Canon + How-to-Run + Infra) and wire an “extreme repo” golden case.
This gives maximum DX wins for minimum engineering surface, and everything is evidence-backed.
