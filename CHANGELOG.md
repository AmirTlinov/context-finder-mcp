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
- Worktree atlas: new `worktree_pack` tool lists git worktrees/branches and summarizes active work
  (HEAD, branch, dirty paths), with deterministic cursor pagination and meaning drill-down actions.

### Changed

- Meaning degradation: under tight budgets, the CP shrink policy preserves multiple
  `ENTRY` points (better behavior for monorepos / workspaces).
