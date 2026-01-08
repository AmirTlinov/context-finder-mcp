# Contributing

Thanks for your interest in contributing to Context Finder.

## Development setup

Requirements:

- Rust (stable)
- A working C/C++ toolchain for native deps (standard Rust setup)

Model assets are optional for most development tasks. For deterministic, model-free tests:

```bash
CONTEXT_EMBEDDING_MODE=stub cargo test --workspace
```

## Quality gates

Run these before opening a PR:

```bash
scripts/validate_contracts.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
CONTEXT_FINDER_EMBEDDING_MODE=stub cargo test --workspace
```

## Documentation

- Documentation is maintained in English (`*.md`).
- Keep command examples consistent with `context --help`.

## Contract-first changes (APIs/integrations)

If your change affects anything another process can call or parse (CLI JSON, HTTP, gRPC, MCP tools):

1. Update the contract first (`contracts/…` and/or `proto/…`).
2. Implement the change.
3. Add/adjust tests.
4. Run the quality gates above.

See `contracts/README.md` and `AGENTS.md`.

## Models and caches

- Do not commit downloaded model assets under `models/**`.
- Do not commit local caches (`.context/`, `.fastembed_cache/`, `.deps/`, etc.).

## Benchmarks and datasets

- Bench harness lives under `bench/` (see `bench/README.md`).
- Golden evaluation datasets live under `datasets/`.

## PR hygiene

- Keep changes focused and avoid unrelated refactors.
- Add tests for behavior changes.
- Prefer clear error messages and stable JSON outputs.
