# Changelog

All notable changes to this project are documented in this file.

The format is loosely based on “Keep a Changelog”, but optimized for agent-first tooling:
we prioritize **behavioral deltas**, **contracts**, and **quality gates** over prose.

## Unreleased

### Added

- Meaning mode: `meaning_pack` and `meaning_focus` now provide actionable `next_actions` hints
  in MCP `.context` output when `response_mode=full` (keeps defaults low-noise).
- Meaning eval: expanded the stub smoke dataset with additional repo archetypes
  (monorepo/workspace, “no docs but CI+contracts”, generated/noise-heavy trees).
- Meaning eval: added a polyglot monorepo fixture (Rust + Node + Python) to ensure
  CI-derived canon loop + contract anchors remain reliable across mixed ecosystems.
- Atlas eval: added CI-gated tests for `atlas_pack` meaning determinism and noise suppression.
- Worktree atlas: new `worktree_pack` tool lists git worktrees/branches and summarizes active work
  (HEAD, branch, dirty paths), with deterministic cursor pagination and meaning drill-down actions.
- Onboarding atlas: new `atlas_pack` tool returns a meaning-first CP (canon/CI/contracts/entrypoints)
  plus a bounded worktree overview in one call (optimized for agent onboarding).

### Changed

- Meaning degradation: under tight budgets, the CP shrink policy preserves multiple
  `ENTRY` points (better behavior for monorepos / workspaces).
- Meaning map: suppress `.worktrees/` from `S MAP` (treated as a workspace worktree store; avoids
  “branch checkout mass” dominating structure maps).
- Meaning trust: in `response_mode=facts|full`, `meaning_pack` / `meaning_focus` now emit a compact
  evidence-coverage hint (`coverage: anchors_ev=… steps_ev=… ev=…`) to help agents judge trust
  without expanding token budgets; `atlas_pack` emits the same signal as `meaning_coverage`.
- Meaning onboarding: under tight budgets, `meaning_pack` now keeps external `BOUNDARY` claims
  (HTTP/CLI/events/DB) when they are detected, even if the query doesn’t explicitly say
  “infra/boundary” (stays low-noise for library-only repos).
- Meaning degradation: emit the general sense map (`S MAP`) before `S OUTPUTS` so the
  repo-wide orientation survives truncation longer than artifact-heavy areas.
- Worktree atlas: in `response_mode=full`, `worktree_pack` now includes a bounded, evidence-backed
  purpose summary per worktree (canon loop + anchors like CI/contracts) and suggests an
  `evidence_fetch` follow-up for quick verification.
- Worktree atlas: purpose summaries now also include `touched_areas` (best-effort zones derived from
  dirty paths, like `interfaces`/`ci`/`core`) so an agent can scan “what this branch is about”.
- Worktree atlas: `touched_areas` now correctly treats untracked directory entries (e.g. `contracts/`)
  as a zone signal (common when `status.showUntrackedFiles=normal`).
- Worktree atlas: `touched_areas` is now also derived from committed branch diffs vs a best-effort
  base ref (e.g. `main`), so clean feature branches still show “what they’re about”.
- Worktree atlas: `worktree_pack` now includes deterministic activity and divergence hints
  (`last_commit_date`, `ahead`, `behind`) and uses activity in ranking (dirty → last activity → path)
  to surface “what’s active” first without relying on wall-clock time.
- Worktree atlas: `worktree_pack` headers now include a compact `hint:` line with stable tags
  (e.g. `sync_base`, `ahead_of_base`, `uncommitted_changes`, `detached_head`) so agents can
  quickly choose the next move without re-parsing freeform text.
- Worktree atlas: `worktree_pack` now surfaces `touches:` hints already in `response_mode=facts`
  (zones derived from dirty paths and/or committed diffs), so “what this branch is about” is
  visible without paying for full per-worktree meaning summaries.
- Worktree atlas: suppress `.worktrees/` from dirty path samples (worktree storage is workspace
  noise, not a meaningful repo change).
- Worktree atlas: improve large-repo behavior by increasing the internal meaning budget used for
  purpose summaries (prevents “dict-only truncation” that could suppress canon/anchor signals).
- Meaning canon extraction: avoid false positives where `checkout` matched `check`, which could
  incorrectly label CI setup steps as “test” canon.
- Capabilities: the recommended `start` route now points to `atlas_pack` (meaning CP + worktrees)
  instead of `read_pack`.
- Onboarding atlas: in `response_mode=full`, `atlas_pack` now suggests a direct `meaning_pack`
  follow-up for the most relevant worktree (based on worktree ranking), so agents can jump
  straight into “what this branch/worktree is about” without extra navigation.
- Onboarding atlas: `atlas_pack` now lets meaning-mode choose signal-driven map defaults (dataset-heavy
  vs monorepo) instead of hardcoding a fixed map limit.
- Batch: accept legacy `items: string[]` payloads (best-effort parsing) for compatibility with
  older clients, while keeping batch v2 `$ref` support for structured callers.
