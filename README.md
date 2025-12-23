# Context Finder

Semantic code search **built for AI agents**: index once, then ask for **one bounded context pack** you can feed into a model or pipeline.

If you’re tired of “search → open file → search again → maybe the right function?”, Context Finder turns a query into a compact, contract-stable JSON response — with optional graph-aware “halo” context.

## What you get

- **Agent-first output:** `context-pack` returns a single JSON payload bounded by `max_chars`.
- **Stable integration surfaces:** CLI JSON, HTTP, gRPC, MCP — all treated as contracts.
- **Hybrid retrieval:** semantic + fuzzy + fusion + profile-driven boosts.
- **Graph-aware context:** attach related chunks (calls/imports/tests) when you need it.
- **Measured quality:** golden datasets + MRR/recall/latency/bytes + A/B comparisons.
- **Offline-first models:** download once from a manifest, verify sha256, never commit assets.
- **No silent CPU fallback:** CUDA by default; CPU only if explicitly allowed.

## 60-second quick start

### 1) Build and install

```bash
git clone https://github.com/AmirTlinov/context-finder-mcp.git
cd context-finder-mcp

cargo build --release
cargo install --path crates/cli
```

Optional local alias (avoids `cargo install` during iteration):

```bash
alias context-finder='./target/release/context-finder'
```

### 2) Install models (offline) and verify

Model assets are downloaded once into `./models/` (gitignored) from `models/manifest.json`:

```bash
context-finder install-models
context-finder doctor --json
```

Execution policy:

- GPU-only by default (CUDA).
- CPU fallback is allowed only when `CONTEXT_FINDER_ALLOW_CPU=1`.

### 3) Index and ask for a bounded pack

```bash
cd /path/to/project

context-finder index . --json
context-finder context-pack "index schema version" --path . --max-chars 20000 --json --quiet
```

Want exploration with graph expansion?

```bash
context-finder context "streaming indexer health" --path . --strategy deep --show-graph --json --quiet
```

## Integrations

### CLI + JSON Command API

One request shape; one response envelope:

```bash
context-finder command --json '{"action":"search","payload":{"query":"embedding templates","limit":5,"project":"."}}'
```

### HTTP

```bash
context-finder serve-http --bind 127.0.0.1:7700
```

- `POST /command`
- `GET /health`

### gRPC

```bash
context-finder serve-grpc --bind 127.0.0.1:50051
```

### MCP server

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

## Contracts (source of truth)

All integration surfaces are contract-first and versioned:

- [contracts/README.md](contracts/README.md)
- [contracts/command/v1/](contracts/command/v1/) (JSON Schemas)
- [contracts/http/v1/openapi.json](contracts/http/v1/openapi.json) (OpenAPI 3.1)
- [proto/](proto/) (gRPC)

## Documentation

- [docs/README.md](docs/README.md) (navigation hub)
- [docs/QUICK_START.md](docs/QUICK_START.md) (install, CLI, servers, JSON API)
- [USAGE_EXAMPLES.md](USAGE_EXAMPLES.md) (agent-first workflows)
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
- [docs/CONTEXT_PACK.md](docs/CONTEXT_PACK.md)
- [docs/COMMAND_RFC.md](docs/COMMAND_RFC.md)
- [PHILOSOPHY.md](PHILOSOPHY.md)
- [models/README.md](models/README.md)
- [bench/README.md](bench/README.md)

## Development

```bash
scripts/validate_contracts.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
CONTEXT_FINDER_EMBEDDING_MODE=stub cargo test --workspace
```

## License

MIT OR Apache-2.0

## Contributing

See `CONTRIBUTING.md`.
