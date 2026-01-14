# Quick Start: Context Finder MCP

## What is Context Finder MCP?

Context Finder MCP is a semantic code navigation tool designed for AI agents and coding assistants.

Its core UX goal is **not** “yet another search command” — it is to feel like an agent’s **always fresh, bounded project memory**, so daily context gathering does *not* degrade into `rg/cat/grep` loops.

## Installation

### From Source (recommended)

```bash
git clone https://github.com/AmirTlinov/context-finder-mcp.git
cd context-finder-mcp
cargo build --release
cargo install --path crates/cli --locked
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

### MCP (recommended for AI agents)

**You do not need to manually index projects when using MCP.**

Context Finder keeps an **incremental index in the background** and self-heals on semantic tool calls. If semantic search is unavailable (e.g., first warmup or embeddings temporarily unavailable), tools degrade to **filesystem-first fallbacks** so the agent can keep moving.

Start with one of these:

- `atlas_pack` — onboarding atlas: meaning-first CP + worktrees (answers “how to run/verify”, “where are CI gates/contracts/entrypoints”).
- `read_pack` — daily “project memory”: stable repo facts + key snippets under one `max_chars` budget.
- `repo_onboarding_pack` — onboarding: map + a few key docs under one budget.
- `atlas_pack` — onboarding atlas: meaning CP + worktrees; designed to quickly answer “where is the canon loop / CI gates / contracts / entrypoints”.
- `meaning_pack` — meanings-first orientation: a compact “Cognitive Pack (CP)” with evidence pointers (high signal, low tokens).
- `meaning_focus` — semantic zoom-in around a specific file/dir (scoped CP + evidence pointers).
- `worktree_pack` — worktree/branch atlas: list active worktrees and what is being worked on (best when a repo uses `.worktrees/`; in `response_mode=full` includes evidence-backed canon/CI/contracts summary).
- `evidence_fetch` — fetch exact file text for evidence pointers only (verbatim + hash, detects staleness).

Tip: `meaning_pack` and `meaning_focus` support an optional diagram output via `output_format`:

- `output_format=context_and_diagram` — CP text + an `image/svg+xml` “Meaning Graph”.
- `output_format=diagram` — diagram only (lowest token usage; requires an image-capable client/model).

Suggested “semantic zoom” flow:

1) `atlas_pack` (or `meaning_pack`) to get structure, canon loop, and the next best action.
2) (optional) `meaning_focus` when you need to zoom in on a specific area before reading.
3) `evidence_fetch` only for the specific EV pointers you need to verify or implement changes.

Note: to surface the actionable `next_actions` guidance in MCP `.context`, use `response_mode=full`.
Default modes (`facts` / `minimal`) stay intentionally low-noise and omit these hints.

### Multi-session safety

When you have multiple agent sessions (or multiple repos open at once), prefer explicit roots:

- Always pass `path` on tool calls when your client can.
- If `path` is omitted, Context Finder resolves roots in this order:
  1) per-connection session root (from MCP `roots/list`), then
  2) `CONTEXT_FINDER_ROOT` / `CONTEXT_FINDER_PROJECT_ROOT`, then
  3) (non-daemon only) server process cwd fallback.
- If the client reports multiple workspace roots, Context Finder **does not guess** (fail-closed) and requires an explicit `path`.
- Every tool response includes a `root_fingerprint` in tool meta so clients can detect cross-project mixups without exposing filesystem paths.
- For human debugging, `response_mode=full` also prints `N: root_fingerprint=...` in the `.context` text output so you can eyeball provenance quickly.
- Error responses also include this note when a root is known, so provenance is visible even when a call fails.

### CLI (optional; debugging/automation)

The CLI still exposes `index` for explicit rebuilds (useful for automation, debugging, or when you want deterministic “do it now” behavior).

#### 1) Index a Project (manual)

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

#### 2) Search for Code

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

#### 3) Build a Bounded Context Pack (JSON)

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
  --out-json .agents/mcp/context/eval.smoke.json \
  --out-md .agents/mcp/context/eval.smoke.md

context-finder eval-compare . --dataset datasets/golden_smoke.json \
  --a-profile general --b-profile general \
  --a-models bge-small --b-models embeddinggemma-300m \
  --json \
  --out-json .agents/mcp/context/eval.compare.json \
  --out-md .agents/mcp/context/eval.compare.md
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
cargo install --path crates/mcp-server --locked
context-finder-mcp
```

#### Multi-agent mode (shared backend)

If you have many agent sessions open, the MCP server defaults to a shared backend daemon so you don’t pay the cost of a cold start in every session and cursor-only continuation stays stable.

If you need an isolated in-process server per session (mostly useful in tests), disable shared mode:

```text
CONTEXT_FINDER_MCP_SHARED=0
```

In shared mode, each session runs a lightweight stdio proxy that connects to a single long-lived daemon process (`context-finder-mcp daemon`) behind the scenes.

Optional:

- `CONTEXT_FINDER_MCP_SOCKET` overrides the Unix socket path for the daemon.
- Keep the indexing daemon enabled (avoid `CONTEXT_FINDER_DISABLE_DAEMON=1`) if you want indexes to stay warm while you work.
- `CONTEXT_FINDER_INDEX_CONCURRENCY` caps how many projects can be indexed in parallel in shared daemon mode (default: auto; range: 1–32). Requires restarting the daemon to take effect.
- `CONTEXT_FINDER_WARM_WORKER_CAPACITY` caps how many hot project warm workers are kept in the daemon warm-indexer LRU (default: auto). Requires restarting the daemon to take effect.
- `CONTEXT_FINDER_WARM_WORKER_TTL_SECS` sets idle eviction TTL for warm workers (default: auto). Requires restarting the daemon to take effect.
- `CONTEXT_FINDER_ENGINE_SEMANTIC_INDEX_CAPACITY` caps how many semantic indices are kept loaded per project engine (default: auto; minimum: 1). Lower values reduce RAM but may increase on-demand index loads. Requires restarting the daemon to take effect.

Self-audit tool inventory (no MCP client required):

```bash
context-finder-mcp --print-tools
```

#### Codex CLI (MCP) integration

Example `~/.codex/config.toml` snippet:

```toml
[mcp_servers.context-finder]
command = "context-finder-mcp"
args = []

[mcp_servers.context-finder.env]
CONTEXT_FINDER_PROFILE = "quality"
# Shared backend is enabled by default (agent-native multi-session UX).
# Set to "0" only if you need an isolated in-process server per session:
# CONTEXT_FINDER_MCP_SHARED = "0"

# Default output is agent-native `.context` plain text (no JSON in agent chat).
# For machine-readable automation / `$ref` fan-out, use the Command API.
```

Tip: point `command` at an installed binary (release) rather than `cargo run` — otherwise the first
compile can exceed MCP client startup timeouts. If your client still times out on cold start, raise
`startup_timeout_sec` in the MCP config (Codex defaults to ~10s).

Daily “project memory” tool (best default for agents; one call → stable repo facts + key docs):

```jsonc
{ "path": "/path/to/project" }
```

If your agent session is already “inside” the repo (common case), you can omit `path` and let the
shared-backend proxy inject the project root from its current working directory:

```jsonc
{}
```

If you are using Context Finder as a daily navigation/memory tool, start with the playbook:

- `docs/AGENT_MEMORY.md` — `read_pack` as the “apply_patch of context”

This calls `read_pack` with defaults (see the full schema via `context-finder-mcp --print-tools` or the MCP tool schema in `crates/mcp-server/src/tools/schemas/read_pack.rs`):
- `intent: "memory"` (returns a memory pack)
- `response_mode: "facts"` (low-noise daily mode: returns mostly project content; avoids diagnostic meta and helper guidance)
- `auto_index: false` for `memory`/`onboarding` (no `.agents/mcp/context/.context` side effects unless you ask)
- `auto_index: false` by default for `recall`, but per-question `deep` is an explicit opt-in and may auto-index unless you explicitly disable it
- `auto_index: true` for `query` (so semantic packs work out of the box)

To include a graph-based architecture overview (slower, index-backed), request `response_mode: "full"`.

Capabilities tool (`capabilities`): one call returns versions, default budgets, and a recommended
start route for zero-guess onboarding.

One-call reading pack tool (`read_pack`; a single entry point for file/grep/query/onboarding/memory/recall, with cursor-only continuation).
All MCP tool errors are reported in `.context` text (`A: error: <code>` + a short human hint). `read_pack` strictly honors `max_chars`; it is optimized for agent UX:
- the default pack starts with a compact `project_facts` section (stable repo facts)
- defaults to `response_mode: "facts"` for low-noise daily use
- supports `response_mode: "full"` when you explicitly want extra diagnostics / helper guidance
- supports `response_mode: "minimal"` when you want the smallest possible response
- supports one-call recall via `questions` (preferred) or `ask` (single prompt), returning `recall` sections with compact `snippet` payloads
- is **secret-safe by default**: read tools refuse or skip common secret locations unless you explicitly set `allow_secrets: true`
- has a **freshness-safe fallback**: when the semantic index is stale and `auto_index=false`, `read_pack` (query/recall deep) deterministically switches to filesystem strategies instead of returning silently stale semantic results
- can include an **external memory overlay** (e.g., BranchMind) when a project-scoped cache file is present (returned as an `external_memory` section, bounded + low-noise)

External memory overlay (default convention):

- BranchMind context pack file: `.agents/mcp/context/.context/branchmind/context_pack.json` (preferred; legacy `.context/…` and `.context-finder/…` are supported).
- Codex CLI worklog cache: stored under your Codex home (e.g. `~/.codex/.context-finder/external_memory/codex_cli/*`, keyed by project root), derived from `~/.codex/sessions` / `$CODEX_HOME/sessions` (project-scoped via session cwd prefix; deduped + bounded; never written into the repo).

### Recall mini-language (per question)

To keep the request contract simple (no “semantic sugar knobs” in the schema), `read_pack` supports a tiny *per-question* directive syntax inside `questions[]` strings.
These directives are optional and are **not** part of the JSON schema — they are parsed from each question line.

Common directives:

- Routing: `fast` (grep/file-first), `deep` (semantic allowed; can auto-index per question)
- Scoping: `in:<prefix>`, `not:<prefix>` (path prefixes relative to project root)
- File filter: `glob:<pattern>` / `fp:<pattern>`
- File jump: `file:<path[:line]>` / `open:<path[:line]>`
- Grep intent: `re:<regex>` / `regex:<regex>`, `lit:<text>` / `literal:<text>`
- Output control: `k:<N>` (snippets per question), `ctx:<N>` (grep context lines)
- Deep indexing budget: `index:5s`, `deep:8000ms` (auto-index time budget per question)

If a cursor is returned and looks unusually compact, it may rely on short-lived server-side continuation state (agent-friendly, avoids cursor bloat).

If it expires, simply repeat the original call that produced it. In shared-backend mode (the default), cursor aliases are persisted best-effort on disk, so compact `cfcs2:…` cursors typically survive process restarts as long as their TTL has not expired (`cfcs1:…` is legacy).

Safety note (multi-agent): once a session has an established default root, `read_pack` won’t switch projects based on a cursor alone. To switch roots intentionally, pass an explicit `path`.

```jsonc
// Daily long-memory pack (defaults; stable repo facts + key configs/docs under one budget)
{
  "path": "/path/to/project"
}

// One-call recall ("remember" anything about the repo in one call)
// - `questions` is preferred for deterministic, multi-part asks
// - `ask` is a convenience for single free-form questions
{
  "path": "/path/to/project",
  "questions": [
    "Where is the HTTP /command route implemented?",
    "How do I run tests in this repo?",
    "re: cargo test", // optional: explicit grep directive (Rust regex syntax)
    "lit: cargo test", // optional: literal grep directive (no regex)
    "fast in:src lit: cursor_fingerprint", // optional: per-question scoping + force fast path
    "deep index:8s k:5 ctx:20 How does auto-index decide the project root? in:crates" // deep mode + knobs
  ],
  "max_chars": 6000
}

// Read a file window (internally calls file_slice)
{
  "path": "/path/to/project",
  "intent": "file",
  "file": "src/lib.rs",
  "offset": 120,
  "limit": 80,
  "max_chars": 2000,
  "response_mode": "facts",
  "timeout_ms": 12000
}

// Continue without repeating inputs
{
  "cursor": "<cursor>"
}
```

Repo onboarding pack tool (`repo_onboarding_pack`) is still available when you want a richer map-first onboarding view.

Semantic context pack tool (`context_pack`; bounded output; supports path filters):

```jsonc
{
  "path": "/path/to/project",
  "query": "rate limiter",
  "include_paths": ["src"],
  "exclude_paths": ["src/generated"],
  "prefer_code": true,
  "include_docs": false,
  "related_mode": "focus",
  "response_mode": "facts",
  "max_chars": 2000
}
```

Regex context reads tool (`grep_context`; grep `-B/-A/-C` style, merged hunks, bounded output):

```jsonc
{
  "path": "/path/to/project",
  "pattern": "stale_policy",
  // Optional: treat pattern as a literal string (like `rg -F`)
  // "literal": true,
  "file_pattern": "crates/*/src/*",
  "before": 50,
  "after": 50,
  "max_hunks": 40,
  "max_chars": 2000,
  // Optional: "numbered" prefixes each line with "<line>: " and marks match lines as "<line>:* ".
  // Default in low-noise modes is "plain" (more payload under max_chars); line ranges + match_lines are still provided.
  // "format": "numbered",
  "response_mode": "facts"
}
```

Pagination (cursor): when the `.context` output includes an `M: <cursor>` line near the end, call it again with `cursor: "<cursor>"`.
In `response_mode: "full"`, tools include extra diagnostics; error responses may also include compact `next:` hints.

Cursor tokens are opaque and bound to the original query/options (changing them will be rejected).
For some tight-loop tools (notably `file_slice` and `grep_context`), the cursor contains enough info for cursor-only continuation — you do not need to resend the original options.

Most *semantic* MCP tools default to `response_mode: "facts"` and include `meta.index_state` (best-effort) on both success and error responses to expose index freshness.
Some tight-loop read tools (`file_slice`, `grep_context`, `list_files`, `text_search`, `map`) default to `response_mode: "minimal"` to keep output almost entirely project content.
For these tools, `response_mode: "facts"` stays low-noise by design (it strips helper guidance but still avoids heavy diagnostics). Use `response_mode: "full"` when you explicitly want diagnostics / freshness details (including `meta.index_state`).
For example, `map` in `"minimal"` returns mostly directory paths (low noise), while `"full"` can include richer diagnostics.
Use `response_mode: "minimal"` for the smallest possible response. Use `response_mode: "full"` when debugging tool behavior or investigating index freshness.
For semantic tools (`context_pack`, `context`, `impact`, `trace`, `explain`, `overview`),
`auto_index` defaults to true; use `auto_index=false` or `auto_index_budget_ms` to control the
reindex budget. The attempt is reported under `meta.index_state.reindex`.

Batch tool (one MCP call → many tools, bounded output). Output is `.context` text only and strictly capped by `max_chars`.
In `version: 2`, item inputs can depend on earlier outputs via `$ref` (JSON Pointer):

```jsonc
{
  "version": 2,
  "path": "/path/to/project",
  "max_chars": 2000,
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
  "offset": 120,
  "limit": 80,
  "max_chars": 2000
}
```

If the response is truncated, continue with `cursor` (cursor-only continuation):

```jsonc
{
  "cursor": "<cursor>"
}
```

List files tool (bounded file enumeration; designed to replace `ls/find/rg --files` in agent loops):

```jsonc
{
  "path": "/path/to/project",
  "file_pattern": "src/*",
  "limit": 200,
  "max_chars": 2000
}
```

Note: by default, `list_files` omits common secret paths (e.g. `.env`). To include them, set:

```jsonc
{ "allow_secrets": true }
```

## JSON Command API

For programmatic access, use the `command` subcommand:

```bash
# Index project
context-finder command --json '{"action": "index", "payload": {"path": "/project"}}'

# Capabilities handshake (versions + budgets + start route)
context-finder command --json '{"action": "capabilities", "payload": {}}'

# Search
context-finder command --json '{"action": "search", "payload": {"query": "handler", "limit": 5}}'

# From file
context-finder command --file request.json

# From stdin
echo '{"action": "search", "payload": {"query": "test"}}' | context-finder command
```

Errors return `status: "error"` plus an `error` envelope (`code/message/details/hint/next_actions`).
Both success and error responses may include `next_actions` when a follow-up is obvious.

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
| `capabilities` | Return versions, default budgets, and recommended start route |
| `index` | Index a project directory |
| `search` | Semantic code search |
| `search_with_context` | Search with surrounding context |
| `context_pack` | Build a single bounded context pack (best default for agents) |
| `task_pack` | Task-oriented pack: context pack + `why` + `next_actions` |
| `text_search` | Bounded literal search (filesystem-first; cursor-only continuation) |
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
| `--cache-dir` | Cache directory | `.agents/mcp/context/.context/cache` |
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
ls .agents/mcp/context/.context/

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

Expected MCP tool names (18):

- `capabilities`, `help`
- `map`, `repo_onboarding_pack`, `read_pack`
- `file_slice`, `list_files`, `grep_context`, `batch`
- `doctor`, `search`, `context`, `context_pack`
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
