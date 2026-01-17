# Evaluation (Regression-Proof Quality)

Context Finder is not “done” when it feels good on one repo.
It is “done” when quality is **measured**, **repeatable**, and **gated** against regressions.

This doc explains the evaluation loop and how it connects to the contracts-first workflow.

## 1) Two complementary layers

### 1.1) Deterministic CI layer (mandatory)

Runs in CI and must be stable:

- Uses `CONTEXT_FINDER_EMBEDDING_MODE=stub` (deterministic, no model downloads).
- Validates **boundedness**, **determinism**, **fallback behavior**, and **contracts**.
- Uses small, focused golden datasets + synthetic repos in tests.

### 1.2) Real-repo matrix (optional, recommended locally)

Runs on representative repos to catch “it works in prod” issues:

- Different ecosystems (Rust, TS/JS, Python).
- Monorepos, docs-heavy repos, huge files, generated/vendor directories.
- Tracks retrieval quality (MRR/Recall/NDCG), not just correctness.

We also recommend a **real-repo “zoo” for MCP tool UX** (meaning/atlas/worktree packs):

- Runner: `context-mcp-eval-zoo` (package `context-mcp`, binary `context-mcp-eval-zoo`)
- Measures UX regressions: **stability** (same CP twice), **latency**, **noise ratio**, and **token_saved**
- Includes multi-branch/worktree visibility via `worktree_pack` (bounded + deterministic)
- Designed to be safe on messy repos: bounded scanning, noise suppression, and binary-safe baseline estimation
- Outputs: JSON and Markdown summary tables (for tracking over time)
- JSON contract: `contracts/eval/v1/zoo_report.schema.json`

Example (run on your local “projects” directory):  
`cargo run -p context-mcp --bin context-mcp-eval-zoo -- --root "/home/amir/Документы/projects" --limit 20 --out-json /tmp/context_zoo.json --out-md /tmp/context_zoo.md`

Tip: if you want the zoo to also scan repos living under a `.worktrees/` directory (common in research setups), add `--include-worktrees`.

If you want the run to **fail closed** on regressions, use `--strict` (plus optional `--strict-*` thresholds for latency/noise/token_saved).

Real-repo runs are not required for CI to keep the loop fast and offline-friendly.

## 2) Golden datasets (`datasets/*.json`)

### 2.1) Retrieval datasets (CLI eval)

These datasets measure retrieval quality (search/ranking) and are used by the CLI `eval` commands.

#### Format

Datasets are JSON files with schema version `1`:

- `cases[].id`: stable identifier
- `cases[].query`: the query string
- `cases[].expected_paths`: one or more file paths expected to appear in the top-k results
- optional: `cases[].expected_symbols`
- optional: `cases[].intent` (`identifier` / `path` / `conceptual`)

Datasets are used by:

- CLI: `context-finder eval` and `context-finder eval-compare`

#### Philosophy

Golden datasets are for **positive** retrieval: they must specify expected paths.
Negative behaviors (e.g. “must not return unrelated hits”) are validated via **tests**
because they require richer assertions than “path must appear”.

### 2.2) Meaning-mode datasets (CP quality)

Meaning-mode is evaluated separately because its output is not a ranked list — it is a bounded,
evidence-backed “orientation pack” (Cognitive Pack / CP).

We gate meaning-mode quality in CI using:

- Dataset: `datasets/meaning_stub_smoke.json`
- Runner: `crates/mcp-server/tests/meaning_mode.rs` (under `cargo test`, stub mode)

Each case builds a small synthetic repo fixture and validates that the meaning output stays:

- **high-signal** (expected anchors/zones appear),
- **low-noise** (generated/dataset trees do not dominate the map),
- **token-efficient** (`min_token_saved` thresholds),
- **stable** (deterministic output for the same root + query),
- **responsive** (latency stays within reasonable budgets),
- and **stable under truncation** (boundedness and degradation rules).

Meaning dataset fields are intentionally richer than retrieval datasets:

- `expect_paths`: must appear in the CP
- `expect_claims`: expected CP “claim” kinds (e.g. `ENTRY`, `CONTRACT`, `AREA`, `STEP`)
- `expect_step_kinds`: expected canon step kinds (e.g. `test`, `setup`) for stronger gating than `STEP`
- `expect_anchor_kinds`: expected anchor categories (e.g. `ci`, `contract`, `canon`)
- `forbid_map_paths`: paths that must not appear in the CP map (noise budget)
- `max_noise_ratio`: optional cap on map “noise ratio” (fraction of `S MAP` entries under known noise dirs)
- `max_latency_ms`: optional per-case wall-time budget for `meaning_pack` under stub mode
- `min_token_saved`: minimum `token_saved` ratio (prevents “quality by flooding”); baseline is derived
  from evidence slices (`EV ... Lx-Ly`) and also from the top anchor files (first N lines)

### 2.3) Onboarding atlas surfaces (`atlas_pack`, `worktree_pack`)

`atlas_pack` and `worktree_pack` are **product surfaces** (they drive the first 1–3 tool calls an
agent makes). They are evaluated in CI via **synthetic repo tests**, because they combine multiple
subsystems (meaning CP + worktree inspection + next-actions).

We gate them using:

- `crates/mcp-server/tests/atlas_pack.rs` (integration: CP + worktrees + next-actions wiring)
- `crates/mcp-server/tests/atlas_pack_quality.rs` (noise suppression + determinism for meaning)
- `crates/mcp-server/tests/worktree_pack.rs` (worktree listing + purpose summary + evidence follow-up)

The invariants are intentionally **evidence-first**:

- No “what to do next” without an evidence-backed anchor (`evidence_fetch` items).
- Deterministic behavior for the same root + query under bounded budgets.
- Noise budgets prevent dataset/build/output mass from dominating onboarding maps.

## 3) Metrics we gate

At minimum, we track:

- `MRR@k` (top-rank quality)
- `Recall@k` (coverage)
- latency and output bytes (to prevent “quality by flooding”)

For premium reliability (see `docs/QUALITY_CHARTER.md`), we also track:

- wrong-root rate (must be 0)
- anchorless-hit rate (should trend to 0 for anchored queries)
- fallback-rate (should be explainable, not chaotic)

For meaning-mode specifically, we also gate:

- anchor recall by category (CI/contracts/canon/entrypoints)
- map noise suppression (generated/dataset/binary “mass” must not win)
- token efficiency (`token_saved` floors on the stub zoo)

For onboarding atlas surfaces, we gate:

- evidence coverage (next-actions include `evidence_fetch` when anchors exist)
- worktree visibility (worktrees appear with deterministic pagination)
- purpose signal presence (in `response_mode=full`, canon loop + anchor hints render without flooding)

## 4) CI gates (required)

Before any change that affects external behavior:

- contracts validation
- fmt + clippy
- tests in stub mode
- eval smoke dataset

See the canonical gate list in `docs/QUALITY_CHARTER.md`.

## 5) How to extend evaluation safely

When you add a new feature or fix a bug:

1) Add/adjust a golden dataset case (if it’s a positive retrieval behavior).
2) Add/adjust a focused test (if it’s a negative/edge-case behavior).
3) Make the smallest implementation change that fixes root cause.
4) Ensure CI gates remain green.
