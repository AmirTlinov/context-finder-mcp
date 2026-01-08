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

Real-repo runs are not required for CI to keep the loop fast and offline-friendly.

## 2) Golden datasets (`datasets/*.json`)

### Format

Datasets are JSON files with schema version `1`:

- `cases[].id`: stable identifier
- `cases[].query`: the query string
- `cases[].expected_paths`: one or more file paths expected to appear in the top-k results
- optional: `cases[].expected_symbols`
- optional: `cases[].intent` (`identifier` / `path` / `conceptual`)

Datasets are used by:

- CLI: `context-finder eval` and `context-finder eval-compare`

### Philosophy

Golden datasets are for **positive** retrieval: they must specify expected paths.
Negative behaviors (e.g. “must not return unrelated hits”) are validated via **tests**
because they require richer assertions than “path must appear”.

## 3) Metrics we gate

At minimum, we track:

- `MRR@k` (top-rank quality)
- `Recall@k` (coverage)
- latency and output bytes (to prevent “quality by flooding”)

For premium reliability (see `docs/QUALITY_CHARTER.md`), we also track:

- wrong-root rate (must be 0)
- anchorless-hit rate (should trend to 0 for anchored queries)
- fallback-rate (should be explainable, not chaotic)

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

