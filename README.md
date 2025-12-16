# Context Finder

Context Finder is a semantic code search tool for AI agents. It indexes a codebase using tree-sitter AST chunking and ONNX Runtime embeddings, and exposes results via:

- CLI (`context-finder`)
- JSON Command API (`context-finder command`, `context-finder serve-http`, `context-finder serve-grpc`)
- MCP server (`context-finder-mcp`)

## Quick start

### 1) Build and install

```bash
git clone https://github.com/AmirTlinov/context-finder.git
cd context-finder

cargo build --release
cargo install --path crates/cli
```

Optional local alias (avoids `cargo install` during iteration):

```bash
alias context-finder='./target/release/context-finder'
```

### 2) Install models (offline)

Model assets are downloaded once into `./models/` (gitignored) from `models/manifest.json`:

```bash
context-finder install-models
context-finder doctor
```

Execution policy:

- GPU-only by default (CUDA).
- CPU fallback is allowed only when `CONTEXT_FINDER_ALLOW_CPU=1`.

### 3) Index and query

```bash
cd /path/to/project

# Index once
context-finder index . --json

# Best default for agents: one bounded JSON context pack
context-finder context-pack "index schema version" --path . --max-chars 20000 --json --quiet

# Exploratory search (with graph-aware context assembly)
context-finder context "streaming indexer health" --path . --strategy deep --show-graph --json --quiet
```

## Configuration

### Profiles

Select a profile with `--profile <name>` or `CONTEXT_FINDER_PROFILE=<name>`.

- `quality` (default): balanced speed/quality routing.
- `fast`: speed-first (single lightweight model).
- `general`: quality-first (multi-model routing; higher latency).
- `targeted/venorus`: targeted boosts/must-hit rules on top of `general`.

Profiles live under `profiles/` (JSON or TOML).

### Environment variables

- `CONTEXT_FINDER_MODEL_DIR`: model directory root (default: `./models`)
- `CONTEXT_FINDER_EMBEDDING_MODEL`: embedding model id (default: `bge-small`)
- `CONTEXT_FINDER_EMBEDDING_MODE`: `fast` (default) or `stub` (deterministic tests, no GPU/models)
- `CONTEXT_FINDER_PROFILE`: active profile name (default: `quality`)
- `CONTEXT_FINDER_CUDA_DEVICE`: CUDA device id
- `CONTEXT_FINDER_CUDA_MEM_LIMIT_MB`: CUDA EP arena limit in MB
- `CONTEXT_FINDER_ALLOW_CPU`: set to `1` to explicitly allow CPU fallback

Most of these also have CLI overrides (`--model-dir`, `--embed-model`, `--embed-mode`, `--profile`, `--cuda-device`, `--cuda-mem-limit-mb`).

Project-local configuration can be stored in `.context-finder/config.json`.

## JSON Command API

Use the `command` subcommand to execute a JSON request:

```bash
context-finder command --json '{"action":"search","payload":{"query":"embedding templates","limit":5,"project":"."}}'
```

You can also expose the same API over HTTP or gRPC:

```bash
context-finder serve-http --bind 127.0.0.1:7700
context-finder serve-grpc --bind 127.0.0.1:50051
```

See `docs/QUICK_START.md` and `docs/COMMAND_RFC.md` for details.

## MCP server

Install:

```bash
cargo install --path crates/mcp-server
```

Example Codex config (`~/.codex/config.toml`):

```toml
[mcp_servers.context-finder]
command = "context-finder-mcp"
args = []

[mcp_servers.context-finder.env]
CONTEXT_FINDER_PROFILE = "quality"
```

### Background daemon

The CLI can spawn a background daemon (`context-finder daemon-loop`) to keep indexes warm for recently used projects.

- Socket: `~/.context-finder/daemon.sock`
- Default TTL: 5 minutes (configurable via `CONTEXT_FINDER_DAEMON_TTL_MS`)

If auto-start cannot find the CLI binary (non-standard installation), set `CONTEXT_FINDER_DAEMON_EXE=/path/to/context-finder`.

## Documentation

- `docs/QUICK_START.md` (install, CLI, servers, JSON API)
- `USAGE_EXAMPLES.md` (agent-first workflows)
- `docs/ARCHITECTURE.md`
- `docs/CONTEXT_PACK.md`
- `docs/COMMAND_RFC.md`
- `models/README.md`
- `bench/README.md`

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
CONTEXT_FINDER_EMBEDDING_MODE=stub cargo test --workspace
```

## License

MIT OR Apache-2.0

## Contributing

See `CONTRIBUTING.md`.
