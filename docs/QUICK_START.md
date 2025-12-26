# Quick Start: Context Finder MCP

## What is Context Finder MCP?

Context Finder MCP is a semantic code navigation tool designed for AI agents and coding assistants. It indexes codebases using tree-sitter AST parsing and ONNX embeddings, enabling fast semantic search and bounded context packs.

## Installation

### From Source (recommended)

```bash
git clone https://github.com/AmirTlinov/context-finder-mcp.git
cd context-finder-mcp
cargo build --release
cargo install --path crates/cli
```

### Requirements

- Rust 1.75+ (2021 edition)
- ONNX Runtime (via the Rust `ort` crate; CUDA provider by default)
- NVIDIA GPU with CUDA (**required by default**, no silent CPU fallback)
- `protoc` is **not** required on the system (vendored during build)
- `Cargo.lock` is expected to be committed for reproducible builds

### Models (offline)

Model assets are downloaded once into `./models/` (gitignored) using `models/manifest.json`.

```bash
# Run from repo root (or set CONTEXT_FINDER_MODEL_DIR)
context-finder install-models
context-finder doctor
```

## Basic Usage

### 1. Index a Project

```bash
cd ~/my-project
context-finder index .

# Force full reindex (ignore incremental cache)
context-finder index . --force

# Output JSON format
context-finder index . --json

# Multi-model: index all expert models referenced by the active profile
context-finder index . --experts --json

# Add specific models on top (comma-separated)
context-finder index . --experts --models embeddinggemma-300m --json
```

### 2. Search for Code

```bash
# Simple search
context-finder search "error handling"

# Limit results
context-finder search "database query" -n 5

# Include code graph relations
context-finder search "authentication" --with-graph

# JSON output for programmatic use
context-finder search "api endpoint" --json
```

### 3. Build a Bounded Context Pack (agent default)

`context-pack` is a single-call, bounded JSON for agent context: primary hits + related halo under a strict character budget.

```bash
# Implementation-first (prefer code), exclude docs, reduce halo noise
context-finder context-pack "EmbeddingCache" --path . \
  --prefer-code --exclude-docs --related-mode focus \
  --max-chars 20000 --json --quiet
```

Notes:

- `--prefer-code` / `--prefer-docs` controls whether markdown docs are ranked after/before code.
- `--exclude-docs` removes `*.md/*.mdx` from both primary and related items.
- `--related-mode focus` gates related items by query hits; use `--related-mode explore` for broader exploration.

### 4. Get Context for Multiple Queries

```bash
# Aggregate context from multiple queries
context-finder get-context "user authentication" "session management" "jwt tokens"

# JSON output
context-finder get-context "error handling" "logging" --json -n 5
```

Note: `get-context` is a CLI helper that composes multiple `search` requests. The Command API action `get_context` is different: it extracts a window around a specific file and line.

### 5. List Symbols

```bash
# List all symbols in project
context-finder list-symbols .

# Filter by file pattern
context-finder list-symbols . --file "*.rs"

# Filter by symbol type
context-finder list-symbols . --symbol-type function

# JSON output
context-finder list-symbols . --json
```

## Evaluation (golden datasets)

Measure quality instead of guessing: run MRR/recall/latency/bytes on a JSON dataset.

```bash
context-finder eval . --dataset datasets/golden_smoke.json --json \
  --out-json .context-finder/eval.smoke.json \
  --out-md .context-finder/eval.smoke.md

context-finder eval-compare . --dataset datasets/golden_smoke.json \
  --a-profile general --b-profile general \
  --a-models bge-small --b-models embeddinggemma-300m \
  --json \
  --out-json .context-finder/eval.compare.json \
  --out-md .context-finder/eval.compare.md
```

## Server Modes

### HTTP Server (JSON API)

```bash
# Start HTTP server on default port 7700
context-finder serve-http

# Custom bind address
context-finder serve-http --bind 0.0.0.0:8080
```

API endpoint: `POST /command`

Health endpoint: `GET /health`

Example request:
```bash
curl -X POST http://localhost:7700/command \
  -H "Content-Type: application/json" \
  -d '{
    "action": "search",
    "payload": {
      "query": "error handling",
      "limit": 10,
      "project": "/path/to/project"
    }
  }'
```

Example health request:

```bash
curl http://localhost:7700/health
```

### gRPC Server

```bash
# Start gRPC server on default port 50051
context-finder serve-grpc

# Custom bind address
context-finder serve-grpc --bind 0.0.0.0:50052
```

### Background Daemon

```bash
# Keep indexes warm for pinged projects
context-finder daemon-loop
```

### MCP Server (tools)

Install and run the MCP server:

```bash
cargo install --path crates/mcp-server
context-finder-mcp
```

Repo onboarding pack tool (best default for agents; one call → `map` + key docs + `next_actions`):

```jsonc
{
  "path": "/path/to/project",
  "map_depth": 2,
  "docs_limit": 6,
  "max_chars": 20000
}
```

One-call reading pack tool (`read_pack`; a single entry point for file/grep/query/onboarding, with cursor-only continuation):

```jsonc
// Read a file window (internally calls file_slice)
{
  "path": "/path/to/project",
  "intent": "file",
  "file": "src/lib.rs",
  "start_line": 120,
  "max_lines": 80,
  "max_chars": 20000,
  "timeout_ms": 55000
}

// Continue without repeating inputs
{
  "path": "/path/to/project",
  "cursor": "<next_cursor>"
}
```

Regex context reads tool (`grep_context`; grep `-B/-A/-C` style, merged hunks, bounded output):

```jsonc
{
  "path": "/path/to/project",
  "pattern": "stale_policy",
  "file_pattern": "crates/*/src/*",
  "before": 50,
  "after": 50,
  "max_hunks": 40,
  "max_chars": 20000
}
```

Pagination (cursor): when a tool returns `truncated: true` and `next_cursor`, call it again with the same inputs + `cursor: "<next_cursor>"`.

Cursor tokens are opaque and bound to the original query/options (changing them will be rejected).

Most read-oriented MCP tools include `meta.index_state` when available to expose index freshness.
For semantic tools (`context_pack`, `context`, `impact`, `trace`, `explain`, `overview`),
`auto_index` defaults to true; use `auto_index=false` or `auto_index_budget_ms` to control the
reindex budget. The attempt is reported under `meta.index_state.reindex`.

Batch tool (one MCP call → many tools, bounded output). In `version: 2`, item inputs can depend on earlier outputs via `$ref` (JSON Pointer):

```jsonc
{
  "version": 2,
  "path": "/path/to/project",
  "max_chars": 20000,
  "items": [
    { "id": "hits", "tool": "text_search", "input": { "pattern": "stale_policy", "max_results": 1 } },
    {
      "id": "ctx",
      "tool": "grep_context",
      "input": {
        "pattern": "stale_policy",
        "file": { "$ref": "#/items/hits/data/matches/0/file" },
        "before": 40,
        "after": 40
      }
    }
  ]
}
```

Notes:

- `path` is canonical; `project` is accepted as an alias.
- `version` defaults to 2; set `version: 1` to disable `$ref` resolution (legacy).
- Batch v2 requires unique `id` values per item.
- `action/payload` are accepted as aliases for `tool/input` (canonical) to match Command API batch.
- `$ref` pointers resolve against an evaluation context keyed by item `id` (`#/items/<id>/...`, not array indices).
- `$ref` to a failed item is rejected; use `{ "$ref": "...", "$default": <value> }` for optional pointers.
- Command API `batch` uses the same `$ref` wrapper semantics (see `contracts/command/v1/batch.schema.json`).

File slice tool (bounded, root-locked file read; designed to replace ad-hoc `cat`/`sed` in agent loops):

```jsonc
{
  "path": "/path/to/project",
  "file": "src/lib.rs",
  "start_line": 120,
  "max_lines": 80,
  "max_chars": 8000
}
```

If the response is truncated, continue with `cursor` (keep the same limits):

```jsonc
{
  "path": "/path/to/project",
  "file": "src/lib.rs",
  "cursor": "<next_cursor>",
  "max_lines": 80,
  "max_chars": 8000
}
```

List files tool (bounded file enumeration; designed to replace `ls/find/rg --files` in agent loops):

```jsonc
{
  "path": "/path/to/project",
  "file_pattern": "src/*",
  "limit": 200,
  "max_chars": 8000
}
```

## JSON Command API

For programmatic access, use the `command` subcommand:

```bash
# Index project
context-finder command --json '{"action": "index", "payload": {"path": "/project"}}'

# Search
context-finder command --json '{"action": "search", "payload": {"query": "handler", "limit": 5}}'

# From file
context-finder command --file request.json

# From stdin
echo '{"action": "search", "payload": {"query": "test"}}' | context-finder command
```

Cross-cutting options are supported under `options` (freshness policy, budgets, path filters):

```bash
context-finder command --json '{
  "action": "context_pack",
  "options": { "stale_policy": "auto", "max_reindex_ms": 1500, "include_paths": ["src"] },
  "payload": {
    "query": "rate limiter",
    "limit": 6,
    "project": ".",
    "prefer_code": true,
    "include_docs": false,
    "related_mode": "focus"
  }
}'
```

Batch (one request → many actions, one bounded result):

```bash
context-finder command --json '{
  "action": "batch",
  "options": { "stale_policy": "auto", "max_reindex_ms": 1500 },
  "payload": {
    "project": ".",
    "max_chars": 20000,
    "items": [
      { "id": "idx", "action": "index", "payload": { "path": "." } },
      { "id": "pack", "action": "task_pack", "payload": { "intent": "understand the indexing pipeline" } }
    ]
  }
}'
```

Notes:
- `items[].id` is trimmed and must be unique.
- Item payloads support `$ref` wrappers: `{ "$ref": "#/items/<id>/data/..." , "$default": ...? }` (see `contracts/command/v1/batch.schema.json`).
- The MCP server `batch` tool supports the same `$ref` wrapper format in `version: 2` (field names differ: `action/payload` vs `tool/input`).

### Available Actions

| Action | Description |
|--------|-------------|
| `batch` | Execute multiple actions in one request (bounded output, partial success) |
| `index` | Index a project directory |
| `search` | Semantic code search |
| `search_with_context` | Search with surrounding context |
| `context_pack` | Build a single bounded context pack (best default for agents) |
| `task_pack` | Task-oriented pack: context pack + `why` + `next_actions` |
| `text_search` | Bounded literal search (corpus-first; filesystem fallback optional) |
| `compare_search` | Compare multiple search strategies |
| `get_context` | Extract a window around a file + line (symbol-aware) |
| `list_symbols` | List symbols in a file |
| `config_read` | Read configuration |
| `map` | Generate codebase structure map |
| `eval` | Evaluate retrieval quality on a golden dataset |
| `eval_compare` | Compare two profiles/model sets on a golden dataset |

## Configuration

### Global Options

All commands support these options:

| Option | Description | Default |
|--------|-------------|---------|
| `-v, --verbose` | Enable verbose logging | off |
| `--quiet` | Only warnings/errors to stderr | off |
| `--embed-mode` | Embedding backend: `fast` or `stub` | fast |
| `--embed-model` | Override embedding model id | unset |
| `--model-dir` | Model directory (overrides `CONTEXT_FINDER_MODEL_DIR`) | `./models` |
| `--cuda-device` | CUDA device ID | unset |
| `--cuda-mem-limit-mb` | CUDA memory arena limit (MB) | unset |
| `--cache-dir` | Cache directory | `.context-finder/cache` |
| `--cache-ttl-seconds` | Cache TTL in seconds | `86400` |
| `--cache-backend` | Cache backend: `file` or `memory` | `file` |
| `--profile` | Search heuristics profile | `quality` |

### Environment Variables

| Variable | Description |
|----------|-------------|
| `CONTEXT_FINDER_MODEL_DIR` | Model cache directory |
| `CONTEXT_FINDER_EMBEDDING_MODEL` | Embedding model id |
| `CONTEXT_FINDER_CUDA_DEVICE` | CUDA device ID |
| `CONTEXT_FINDER_CUDA_MEM_LIMIT_MB` | CUDA memory limit |
| `CONTEXT_FINDER_EMBEDDING_MODE` | Embedding mode |
| `CONTEXT_FINDER_PROFILE` | Search profile |
| `CONTEXT_FINDER_ALLOW_CPU` | Set to `1` to explicitly allow CPU fallback |

### Search Profiles

Profiles customize search behavior for different use cases:

```bash
# Use a specific profile
context-finder search "query" --profile general

# Profile locations:
# - Built-in: profiles/fast.json, profiles/quality.json, profiles/general.json
# - Custom: profiles/targeted/*.json
```

#### Prompted embeddings (templates)

Profiles can define embedding templates (prompt/prefix) for both queries and indexed documents:

```json
{
  "embedding": {
    "max_chars": 8192,
    "query": { "default": "query: {text}" },
    "document": { "default": "passage: {text}" },
    "graph_node": { "default": "graph: {text}" }
  }
}
```

Supported placeholders: `{text}`, `{path}`, `{language}`, `{chunk_type}`, `{symbol}`, `{qualified_name}`, `{parent_scope}`, `{documentation}`, `{imports}`, `{tags}`, `{bundle_tags}`, `{related_paths}`, `{chunk_id}`, `{start_line}`, `{end_line}`, `{doc_kind}`, `{query_kind}`.

## Output Formats

### Human-readable (default)

```
1. src/api/handler.rs (score: 0.92)
   Symbol: handle_request
   Lines: 15-42

2. src/utils/error.rs (score: 0.87)
   Symbol: ApiError
   Lines: 8-25
```

### JSON (`--json`)

```json
{
  "status": "ok",
  "data": {
    "query": "error handling",
    "results": [
      {
        "file": "src/api/handler.rs",
        "start_line": 15,
        "end_line": 42,
        "symbol": "handle_request",
        "score": 0.92,
        "content": "..."
      }
    ]
  },
  "meta": {
    "duration_ms": 45
  }
}
```

## Integration with AI

### MCP integration

Context Finder can be used as a context provider for AI coding assistants:

```bash
# Start HTTP API
context-finder serve-http --bind 127.0.0.1:7700

# AI agent can query:
curl -X POST http://localhost:7700/command \
  -d '{"action": "search", "payload": {"query": "user authentication"}}'
```

### Programmatic Use

The project is organized as a Rust workspace with reusable crates:

- `context-code-chunker` - AST-aware code chunking
- `context-vector-store` - Vector storage and embeddings
- `context-search` - Hybrid search (semantic + fuzzy)
- `context-graph` - Code relationship graph
- `context-indexer` - Project indexing

## Troubleshooting

### Slow Indexing

```bash
# Use stub embedding mode for testing
context-finder index . --embed-mode stub
```

### Out of Memory

```bash
# Limit CUDA memory
context-finder index . --cuda-mem-limit-mb 2048
```

### No Results

```bash
# Check if index exists
ls .context-finder/

# Force reindex
context-finder index . --force
```

### MCP: "tool not found" / missing tools

If your MCP client reports `tool not found` or does not show tools like `read_pack` / `grep_context`, you are almost always running the wrong binary or an old install.

Checklist:

1) Ensure the MCP server command is `context-finder-mcp` (not the CLI `context-finder`).
2) Reinstall the MCP server (updates `~/.cargo/bin/context-finder-mcp`):

```bash
cargo install --path crates/mcp-server --locked --force
```

3) Restart your MCP client (many clients cache the tool inventory on startup).
4) Self-check the server's tool inventory from source:

```bash
CONTEXT_FINDER_EMBEDDING_MODE=stub cargo test -p context-finder-mcp --test mcp_smoke
```

Expected MCP tool names (17):

- `map`, `repo_onboarding_pack`, `read_pack`
- `file_slice`, `list_files`, `grep_context`, `batch`
- `doctor`, `index`, `search`, `context`, `context_pack`
- `text_search`, `explain`, `impact`, `trace`, `overview`

## Development checks

```bash
scripts/validate_contracts.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
CONTEXT_FINDER_EMBEDDING_MODE=stub cargo test --workspace

# "Big audit" (includes contract-first gate + the checks above + extra reports)
./audit.sh

# Optional: strict clippy (core targets: `context-finder-mcp` binary; enabled = gate)
AUDIT_STRICT_CLIPPY=1 ./audit.sh
```

## Documentation

- [Architecture](ARCHITECTURE.md) - Technical details
- [README](../README.md) - Project overview
