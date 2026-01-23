# Repository Rules (Agent-First)

This repo is optimized for **AI agents** and **contract-first integrations**.

If you are about to change behavior that another process can call or parse (CLI JSON, HTTP, gRPC, MCP tools):
follow the rules below.

## 1) Contract-first (hard rule)

- Update contracts first under `contracts/**` (and/or `proto/**`).
- Breaking changes require a new version line under `contracts/<surface>/v(N+1)/`.

See `AGENTS.md` for the source-of-truth map.

## 2) Quality gates (must be green)

Run these before claiming “done”:

```bash
scripts/validate_contracts.sh
bash scripts/structural_guardrails.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
CONTEXT_EMBEDDING_MODE=stub cargo test --workspace
```

If you need the full “daily driver” checklist, see `docs/QUALITY_CHARTER.md`.

## 3) Determinism by default

- Prefer stub mode in tests/CI: `CONTEXT_EMBEDDING_MODE=stub`.
- Do not introduce silent correctness fallbacks (see `docs/QUALITY_CHARTER.md`).

## 4) Repo hygiene (no junk in git)

- Never commit model assets under `models/**`.
- Never commit local caches:
  - `.agents/mcp/context/.context/` (preferred cache dir)
  - legacy `.context/`, `.context-finder/`
  - `.fastembed_cache/`, `.deps/`, etc.

## 5) Structure and ownership

- New responsibility => new module (wiring only in the top-level entrypoint).
- Avoid “misc/utils/common” dumps; prefer a named, owned module.
- Large files are guarded by `scripts/structural_guardrails.txt`.

## Pointers

- Contributor rules + contracts map: `AGENTS.md`
- DX runbook (golden workflows): `docs/AGENT_DX_RUNBOOK.md`
- Product invariants + SLOs: `docs/QUALITY_CHARTER.md`
- Delivery process: `docs/RELEASE_TRAIN.md`
