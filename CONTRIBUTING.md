# Contributing

Thanks for your interest in contributing to Context.

## Development setup

Requirements (Rust is pinned via `rust-toolchain.toml`):

- Rust
- A working C/C++ toolchain for native deps (standard Rust setup)

Model assets are optional for most development tasks. For deterministic, model-free tests:

```bash
CONTEXT_EMBEDDING_MODE=stub cargo test --workspace
```

## Quality gates

Run these before opening a PR:

```bash
bash scripts/validate_quality.sh

# Optional (real embeddings smoke; requires models + CUDA/ORT, or CPU fallback via CONTEXT_ALLOW_CPU=1):
bash scripts/validate_real_embeddings.sh
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
- Do not commit local caches (`.agents/mcp/.context/` (preferred), legacy `.agents/mcp/context/.context/`, `.context/` / `.context-finder/`, `.fastembed_cache/`, `.deps/`, etc.).

## Benchmarks and datasets

- Bench harness lives under `bench/` (see `bench/README.md`).
- Golden evaluation datasets live under `datasets/`.

## PR hygiene

- Keep changes focused and avoid unrelated refactors.
- Add tests for behavior changes.
- Prefer clear error messages and stable JSON outputs.
