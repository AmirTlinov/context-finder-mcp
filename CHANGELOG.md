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
- Atlas eval: added CI-gated tests for `atlas_pack` meaning determinism and noise suppression.
- Worktree atlas: new `worktree_pack` tool lists git worktrees/branches and summarizes active work
  (HEAD, branch, dirty paths), with deterministic cursor pagination and meaning drill-down actions.
- Onboarding atlas: new `atlas_pack` tool returns a meaning-first CP (canon/CI/contracts/entrypoints)
  plus a bounded worktree overview in one call (optimized for agent onboarding).

### Changed

- Meaning degradation: under tight budgets, the CP shrink policy preserves multiple
  `ENTRY` points (better behavior for monorepos / workspaces).
- Worktree atlas: in `response_mode=full`, `worktree_pack` now includes a bounded, evidence-backed
  purpose summary per worktree (canon loop + anchors like CI/contracts) and suggests an
  `evidence_fetch` follow-up for quick verification.
- Worktree atlas: improve large-repo behavior by increasing the internal meaning budget used for
  purpose summaries (prevents “dict-only truncation” that could suppress canon/anchor signals).
- Meaning canon extraction: avoid false positives where `checkout` matched `check`, which could
  incorrectly label CI setup steps as “test” canon.
- Capabilities: the recommended `start` route now points to `atlas_pack` (meaning CP + worktrees)
  instead of `read_pack`.
- Onboarding atlas: `atlas_pack` now lets meaning-mode choose signal-driven map defaults (dataset-heavy
  vs monorepo) instead of hardcoding a fixed map limit.
- Batch: accept legacy `items: string[]` payloads (best-effort parsing) for compatibility with
  older clients, while keeping batch v2 `$ref` support for structured callers.
