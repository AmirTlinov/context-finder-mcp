# Context MCP

Semantic code navigation **built for AI agents**: one call in, one **bounded** pack out — designed to feel like an agent’s “project memory” instead of a pile of `rg/cat/grep` steps.

If you’re tired of “search → open file → search again → maybe the right function?”, Context turns a query into a compact, bounded pack — **agent-native `.context` plain text via MCP**, and **contract-first JSON via the Command API** (CLI/HTTP/gRPC) when you need strict programmatic parsing.

## Start here

- Daily “project memory” UX: `docs/AGENT_MEMORY.md` (`read_pack` playbook)
- Install + run + integrations: `docs/QUICK_START.md`
- The UX/product goals: `PHILOSOPHY.md`
- Premium quality gates (prevents regressions): `docs/QUALITY_CHARTER.md`
- Repo structure + hard rules (for agents): `REPO_RULES.md`
- How we ship without breaking trust: `docs/RELEASE_TRAIN.md`
- Behavioral deltas by release: `CHANGELOG.md`

## Agent UX: “project memory” (the whole point)

Context is meant to be **more convenient than shell probing** *by design*:

- **One entry point for daily use:** `read_pack` (MCP) is the “apply_patch of context”: one call returns stable project facts + relevant snippets under one budget.
- **Facts-first + budget-first:** responses start with compact `project_facts` and strictly honor `max_chars`.
- **Anchored snippets:** memory packs try to jump into the most useful part of long docs/configs (tests/run/config headings), not just the top of the file.
- **Cursor-first continuation:** if it doesn’t fit, you continue with `cursor` — `read_pack` supports cursor-only continuation, and cursors are kept compact (server-backed when needed) to avoid blowing your context window.
- **Noise-zero by default:** default `response_mode: "facts"` (or `"minimal"`) keeps output mostly *project content*, not tool chatter.
- **Safe defaults:** root-locked file IO + conservative secret denylist; hidden configs are indexed only via allowlist (no accidental `.env` leaks; opt-in via `allow_secrets: true` when you explicitly need it).
- **Multi-agent friendly:** shared MCP backend is the default (one warm engine cache + cursor store across many sessions). In shared-daemon mode the server **fails closed** if it cannot resolve a single project root (no guessing from relative hints), to prevent cross-project contamination. Set `CONTEXT_FINDER_MCP_SHARED=0` only if you explicitly want an isolated per-session server (mostly useful in tests).

## What you get

- **Agent-first output:** MCP tools return a single bounded `.context` payload under `max_chars` (high payload density, low tool chatter).
- **Legend on demand:** MCP `help` explains the `.context` envelope (`A:/R:/N:/M:`); `[LEGEND]` is only emitted by `help` to keep other tools low-noise.
- **Help topics:** `help {"topic":"tools"}` lists the tool inventory; `help {"topic":"cheat"}` is a quick usage cheat-sheet; `help {"topic":"budgets"}` explains recommended `max_chars` presets.
- **One-call orchestration:** MCP `batch` runs multiple tools under one bounded `.context` response (partial success per item). For machine-readable batching and `$ref` workflows, use the Command API `batch`.
- **Safe file reads:** MCP `cat` returns a bounded file window (root-locked, line-based, hashed).
- **Regex context reads:** MCP `rg` returns all regex matches with `before/after` context (grep `-B/-A/-C`), merged into compact hunks under hard budgets.
- **Convenience aliases:** MCP `grep` → `rg`, and MCP `find` → `ls` (same behavior; just muscle-memory names).
- **Safe file listing:** MCP `ls` returns bounded file paths (glob/substring filter).
- **Repo onboarding pack:** MCP `repo_onboarding_pack` returns `tree` + key docs (`cat`) in one bounded response. It trims structure before docs under tight budgets, auto-refreshes the index by default, and reports `docs_reason` when no docs were included.
- **One-call reading pack:** MCP `read_pack` is the single entry point for daily “project memory”, targeted reads (`file`/`grep`/`query`), and one-call recall (`questions`/`ask`). By default it returns a compact `project_facts` section + `snippet` payloads under one `max_chars` budget; richer graph/overview output is opt-in.
- **Cursor pagination:** when truncated, MCP tools include an `M: <cursor>` line in `.context` output so agents can continue without guessing.
- **Freshness when you ask for it:** semantic tools can report index freshness via `meta.index_state` (and reindex attempts) without polluting tight-loop reads; use `response_mode: "full"` when you need diagnostics.
- **Stable integration surfaces:** CLI JSON, HTTP, gRPC, MCP — all treated as contracts.
- **Hybrid retrieval:** semantic + fuzzy + fusion + profile-driven boosts.
- **Graph-aware context:** attach related chunks (calls/imports/tests) when you need it.
- **Task packs:** `task_pack` adds `why` + `next_actions` on top of `context_pack`.
- **Bounded text search:** `text_search` is filesystem-first (agent-native `rg` replacement) and stays safe/low-noise under tight budgets; corpus support is kept only for cursor compatibility.
- **Measured quality:** golden datasets + MRR/recall/latency/bytes + A/B comparisons.
- **Offline-first models:** download once from a manifest, verify sha256, never commit assets.
- **No silent CPU fallback:** CUDA by default; CPU only if explicitly allowed.

## 60-second quick start

### 1) Build and install

```bash
git clone https://github.com/AmirTlinov/context-mcp.git
cd context-mcp

cargo build --release
cargo install --path crates/cli --locked
```

Optional local alias (avoids `cargo install` during iteration):

```bash
alias context='./target/release/context'
```

### 2) Install models (offline) and verify

Model assets are downloaded once into `./models/` (gitignored) from `models/manifest.json`:

```bash
context install-models
context doctor --json
```

Execution policy:

- GPU-only by default (CUDA).
- CPU fallback is allowed only when `CONTEXT_ALLOW_CPU=1`.

### 3) Index and ask for a bounded pack

```bash
cd /path/to/project

context index . --json
context context-pack "index schema version" --path . --max-chars 20000 --json --quiet
```

Note: in MCP mode you typically don’t run `index` manually — indexing is triggered automatically and kept fresh incrementally in the background.

Want exploration with graph expansion?

```bash
context context "streaming indexer health" --path . --strategy deep --show-graph --json --quiet
```

## Integrations

### CLI + JSON Command API

One request shape; one response envelope:

```bash
context command --json '{"action":"search","payload":{"query":"embedding templates","limit":5,"project":"."}}'
```

Task-oriented pack with freshness guard and path filters:

```bash
context command --json '{
  "action":"task_pack",
  "options":{"stale_policy":"auto","max_reindex_ms":1500,"include_paths":["src"]},
  "payload":{"intent":"refresh watermark policy","project":".","max_chars":20000}
}'
```

Batch (one request → many actions):

```bash
context command --json '{
  "action":"batch",
  "options":{"stale_policy":"auto","max_reindex_ms":1500},
  "payload":{
    "project":".",
    "max_chars":20000,
    "items":[
      {"id":"idx","action":"index","payload":{"path":"."}},
      {"id":"pack","action":"context_pack","payload":{"query":"stale policy gate","limit":6}}
    ]
  }
}'
```

Notes:
- `items[].id` is trimmed and must be unique.
- Item payloads support `$ref` wrappers: `{ "$ref": "#/items/<id>/data/..." , "$default": ...? }` (see `contracts/command/v1/batch.schema.json`).

### HTTP

```bash
context serve-http --bind 127.0.0.1:7700
```

- `POST /command`
- `GET /health`

### gRPC

```bash
context serve-grpc --bind 127.0.0.1:50051
```

### MCP server

```bash
cargo install --path crates/mcp-server --locked
```

Self-audit tool inventory (no MCP client required):

```bash
context-mcp --print-tools
```

Example Codex config (`~/.codex/config.toml`):

```toml
[mcp_servers.context]
command = "context-mcp"
args = []

[mcp_servers.context.env]
CONTEXT_PROFILE = "quality"
# Shared MCP backend is enabled by default (agent-native multi-session UX).
# Set to "0" only if you need an isolated in-process server per session:
# CONTEXT_MCP_SHARED = "0"

# Optional:
# CONTEXT_MODEL_DIR = "/path/to/models"
# CONTEXT_MCP_SOCKET = "/tmp/context-mcp.sock"
# CONTEXT_MCP_LOG = "1" # stderr-only logs (keep off by default for protocol purity)

# Default output is agent-native `.context` plain text (no JSON in chat).
# `structured_content` is intentionally omitted to keep agent context windows clean.
```

Daily project memory (one MCP call → stable repo facts + key docs): use `read_pack`:

```jsonc
{ "path": "/path/to/project" }
```

Map-first onboarding (one MCP call → map + key docs; use `response_mode: "full"` for extra diagnostics): use `repo_onboarding_pack`:

```jsonc
{
  "path": "/path/to/project",
  "map_depth": 2,
  "docs_limit": 6,
  "response_mode": "full",
  "max_chars": 6000
}
```

Want one MCP tool to replace `cat`/`sed`, `rg -C`, *and* semantic packs? Use `read_pack`:

```jsonc
// Daily memory pack (defaults)
{ "path": "/path/to/project" }

// One-call recall (ask multi-part questions in one MCP call)
{
  "path": "/path/to/project",
  "questions": [
    "Where is the HTTP /command route implemented?",
    "How do I run tests in this repo?",
    "re: cargo test", // optional: explicit grep directive (Rust regex syntax)
    "lit: cargo test", // optional: literal grep directive (no regex)
    "fast in:src lit: cursor_fingerprint", // optional: per-question scoping + force fast path
    "deep index:8s k:5 ctx:20 How does auto-index decide the project root? in:crates", // per-question deep mode + knobs
    "deep How does auto-index decide the project root?" // optional: per-question deep mode (semantic + index if needed)
  ],
  "max_chars": 6000
}

// Read a file window (cat)
{
  "path": "/path/to/project",
  "intent": "file",
  "file": "src/lib.rs",
  "start_line": 120,
  "max_lines": 80,
  "max_chars": 2000
}

// Continue without repeating inputs (cursor-only continuation)
{
  "path": "/path/to/project",
  "cursor": "<cursor>"
}
```

Need grep-like reads with N lines of context across a repo (without `rg` + `sed` loops)? Use `rg`:

```jsonc
{
  "path": "/path/to/project",
  "pattern": "stale_policy",
  // Optional: treat `pattern` as a literal string (like `rg -F`)
  // "literal": true,
  "file_pattern": "crates/*/src/*",
  "before": 50,
  "after": 50,
  "max_hunks": 40,
  "max_chars": 2000,
  // Optional: "numbered" (default) prefixes each line with "<line>: " and marks match lines as "<line>:* "
  // "format": "numbered"
}
```

If the output is truncated, the `.context` text includes an `M: <cursor>` line. `rg` supports cursor-only continuation:

```jsonc
{ "path": "/path/to/project", "cursor": "<cursor>" }
```

Agent-friendly tip: the MCP tool `batch` lets you execute multiple tools in one call (one bounded `.context` response). `path` is canonical (alias: `project`). In batch `version: 2`, item inputs can depend on earlier outputs via `$ref` (JSON Pointer):

```jsonc
{
  "version": 2,
  "path": "/path/to/project",
  "max_chars": 2000,
  "items": [
    { "id": "hits", "tool": "text_search", "input": { "pattern": "stale_policy", "max_results": 1 } },
    {
      "id": "ctx",
      "tool": "rg",
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

When you need the *exact* contents of a file region (without `cat`/`sed`), use the MCP tool `cat`:

```jsonc
{
  "path": "/path/to/project",
  "file": "src/lib.rs",
  "start_line": 120,
  "max_lines": 80,
  "max_chars": 2000
}
```

If the response is truncated, continue with `cursor`:

```jsonc
{
  "path": "/path/to/project",
  "cursor": "<cursor>",
  "max_chars": 2000
}
```

When you need file paths first (without `ls/find/rg --files`), use `ls`:

```jsonc
{
  "path": "/path/to/project",
  "file_pattern": "src/*",
  "limit": 200,
  "max_chars": 2000
}
```

## Contracts (source of truth)

All integration surfaces are contract-first and versioned:

- [contracts/README.md](contracts/README.md)
- [contracts/command/v1/](contracts/command/v1/) (JSON Schemas)
- [contracts/http/v1/openapi.json](contracts/http/v1/openapi.json) (OpenAPI 3.1)
- [proto/](proto/) (gRPC)

## Documentation

- [docs/README.md](docs/README.md) (navigation hub)
- [docs/AGENT_MEMORY.md](docs/AGENT_MEMORY.md) (`read_pack` as “project memory” and daily default)
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
