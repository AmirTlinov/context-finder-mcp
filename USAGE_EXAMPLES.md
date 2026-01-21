# Context Finder — Usage Examples (agent-first)

This document focuses on agent-friendly workflows. In MCP mode, **indexing is automatic and incremental** (no manual “index step” for the agent). The CLI examples below keep `index` because it’s useful for automation/debugging and “force rebuild” scenarios.

## Quick start

### 1) Build/install

```bash
cargo build --release
cargo install --path crates/cli --locked

# (optional) local alias instead of install
alias context-finder='./target/release/context-finder'
```

### 2) Models (offline, `./models`)

Models are downloaded once into `./models/` from `models/manifest.json` and are not committed to git.

```bash
# run from repo root (or use --model-dir / CONTEXT_FINDER_MODEL_DIR)
context-finder install-models
context-finder doctor --json
```

v1 roster (model_id):
- `bge-small` — fast baseline (384d)
- `multilingual-e5-small` — multilingual fallback (384d)
- `bge-base` — higher quality (768d)
- `nomic-embed-text-v1` — long-context doc queries (768d, max_len=8192)
- `embeddinggemma-300m` — "promptable" conceptual queries (768d)

Execution policy: GPU-only by default. CPU fallback only with `CONTEXT_FINDER_ALLOW_CPU=1`.

## Indexing

```bash
# Index the current project using the active profile + embedding model
context-finder index . --json

# Force full reindex (ignore incremental cache)
context-finder index . --force --json

# Multi-model: index all expert models referenced by the profile
context-finder index . --experts --json

# Add specific models on top (comma-separated)
context-finder index . --experts --models embeddinggemma-300m --json
```

## Search and context for agents

### 1) Best default: bounded `context-pack`

```bash
context-finder context-pack "index schema version" --path . --max-chars 20000 --json --quiet
```

Note: `ContextPackOutput` may include `meta.index_state` (best-effort index freshness snapshot).

Tuning knobs for agent workflows:

```bash
# Implementation-first (code-first), exclude docs, reduce halo noise
context-finder context-pack "apexd" --path . \
  --prefer-code --exclude-docs --related-mode focus \
  --max-chars 20000 --json --quiet

# Docs-first (keep markdown, broader exploration)
context-finder context-pack "ARCHITECTURE.md" --path . \
  --prefer-docs --related-mode explore \
  --max-chars 20000 --json --quiet
```

Default profile is `quality` (balanced). For maximum speed: `--profile fast`. For maximum quality: `--profile general`.

### 2) Exploration: `context` (semantic + graph)

```bash
context-finder context "StreamingIndexer health" --path . --strategy deep --show-graph --json --quiet
```

### 3) Fast search: `search`

```bash
context-finder search "embedding templates" --path . -n 10 --with-graph --json --quiet
```

## Project navigation

```bash
# Structure overview (directories/coverage/top symbols)
context-finder map . -d 2 --json --quiet

# Symbols in a file (fast index-backed glob mode)
context-finder list-symbols . --file "crates/cli/src/lib.rs" --json --quiet
```

## Quality evaluation (golden datasets)

```bash
# Evaluate MRR/recall/latency/bytes + artifacts
context-finder eval . --dataset datasets/golden_smoke.json --cache-mode warm \
  --out-json .agents/mcp/context/eval.smoke.json \
  --out-md .agents/mcp/context/eval.smoke.md \
  --json

# A/B comparison across profiles/model sets
context-finder eval-compare . --dataset datasets/golden_smoke.json \
  --a-profile general --b-profile general \
  --a-models bge-small --b-models embeddinggemma-300m \
  --out-json .agents/mcp/context/eval.compare.json \
  --out-md .agents/mcp/context/eval.compare.md \
  --json
```

## Integration examples

### JSON Command API: `action=batch` (one round-trip)

Use `batch` when an agent needs multiple pieces of information but you still want **one bounded JSON response**.

```bash
context-finder command --json '{
  "action":"batch",
  "options":{"stale_policy":"auto","max_reindex_ms":1500},
  "payload":{
    "project":".",
    "max_chars":20000,
    "items":[
      {"id":"idx","action":"index","payload":{"path":"."}},
      {"id":"pack","action":"task_pack","payload":{"intent":"locate the indexing pipeline","max_chars":12000}},
      {"id":"map","action":"map","payload":{"depth":2,"limit":40}}
    ]
  }
}'
```

`$ref` dependencies between items (id-based JSON Pointer into prior item results):

```bash
context-finder command --json '{
  "action":"batch",
  "payload":{
    "project":".",
    "max_chars":20000,
    "items":[
      {"id":"idx","action":"index","payload":{}},
      {"id":"hits","action":"text_search","payload":{"pattern":"stale_policy","max_results":1}},
      {"id":"ctx","action":"get_context","payload":{
        "file":{"$ref":"#/items/hits/data/matches/0/file"},
        "line":{"$ref":"#/items/hits/data/matches/0/line"},
        "window":20
      }}
    ]
  }
}'
```

Notes:
- The outer `options` apply to all items (freshness policy, budgets, filters).
- Item results are independent (`status: ok|error`); the batch itself can still be `ok` (partial success).
- `items[].id` is trimmed and must be unique within the batch.
- `$ref` is recognized only as `{ "$ref": "...", "$default": ...? }` (exact wrapper), and `$ref` to a failed item’s `data` is rejected (use `$default` for fallback).

### Python: one call → context pack

```python
import json
import subprocess

def context_pack(query: str, project: str = ".", max_chars: int = 20000) -> dict:
    proc = subprocess.run(
        [
            "context-finder",
            "context-pack",
            query,
            "--path",
            project,
            "--max-chars",
            str(max_chars),
            "--json",
            "--quiet",
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(proc.stdout)

pack = context_pack("graph_nodes channel")
print(pack["data"]["budget"])
print(pack["data"]["items"][0]["file"])
```

### Node.js: semantic search (JSON)

```ts
import { execFile } from "node:child_process";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);

async function search(query: string, project = ".", limit = 10) {
  const { stdout } = await execFileAsync("context-finder", [
    "search",
    query,
    "--path",
    project,
    "-n",
    String(limit),
    "--json",
    "--quiet",
  ]);
  return JSON.parse(stdout);
}

const res = await search("embedding templates");
console.log(res.data.results.length);
```

## Where to tune quality

- `profiles/quality.json` — default routing + embedding templates (query/doc/doc_kind)
- `profiles/general.json` — "deep/multi" profile (higher latency for quality)
- `models/manifest.json` — model roster + assets (sha256), downloaded into `./models/`
- `datasets/*.json` — golden datasets for objective tuning

## MCP workflows (bounded agent I/O)

The MCP server is designed to replace ad-hoc repo probing (`ls`, `rg`, `sed`) with a few bounded calls.

### 1) Repo onboarding in one call: `repo_onboarding_pack`

Use this when you want a richer **map-first onboarding** view (as opposed to `read_pack`, which is the daily default “project memory” entry point).
Set `response_mode: "full"` if you want helper `next_actions` for guided continuation.

```jsonc
{
  "path": "/path/to/project",
  "map_depth": 2,
  "docs_limit": 6,
  "response_mode": "full",
  "max_chars": 20000
}
```

### 2) One-call reading pack (file/grep/query): `read_pack`

Use `read_pack` as the **daily default** when you want “project memory” instead of shell probing.
It’s intended to make `rg/cat/grep` feel unnecessary by bundling the common loops into one bounded response:
stable `project_facts` first, then relevant snippets — and pagination via `cursor` when needed.

For the exact request/response fields, treat the MCP schema as the source of truth:
`crates/mcp-server/src/tools/schemas/read_pack.rs`.

Read a file window (internally calls `cat`):

```jsonc
{
  "path": "/path/to/project",
  "intent": "file",
  "file": "src/lib.rs",
  "start_line": 120,
  "max_lines": 80,
  "max_chars": 20000
}
```

Continue without repeating inputs:

```jsonc
{
  "path": "/path/to/project",
  "cursor": "<next_cursor>"
}
```

#### One-call recall (“remember what I need”)

Recall mode is where `read_pack` starts to feel like an agent’s memory: you ask multiple focused questions
in one call, and it returns a small set of relevant `snippet`s per question under a shared budget.

```jsonc
{
  "path": "/path/to/project",
  "max_chars": 20000,
  "questions": [
    "Where is the main entrypoint? in:src k:3",
    "How do I run tests? not:target k:2 ctx:12",
    "lit: cargo test ctx:20",
    "deep index:8s in:crates k:5 How does auto-index and root resolution work?"
  ]
}
```

Recall supports a tiny per-question directive syntax inside `questions[]` strings (these are **not** schema fields):
- `fast` / `deep` — force fast grep/file routing vs allow semantic + index
- `in:<prefix>` / `not:<prefix>` — scope candidate paths
- `glob:<pattern>` / `fp:<pattern>` — file pattern hint
- `file:<path[:line]>` / `open:<path[:line]>` — jump to a specific file (optionally with `:line`)
- `re:<regex>` / `lit:<text>` — explicit grep intent (regex vs literal)
- `k:<N>` — snippets per question (bounded)
- `ctx:<N>` — grep context lines per snippet (bounded)
- `index:5s` / `deep:8000ms` — per-question auto-index budget (deep mode)

Read all regex matches with N lines of context (internally calls `rg`):

```jsonc
{
  "path": "/path/to/project",
  "intent": "grep",
  "pattern": "stale_policy",
  "file_pattern": "crates/*/src/*",
  "before": 50,
  "after": 50,
  "max_chars": 20000
}
```

Build a bounded semantic context pack (internally calls `context_pack`):

```jsonc
{
  "path": "/path/to/project",
  "intent": "query",
  "query": "stale policy gate",
  "prefer_code": true,
  "include_docs": false,
  "max_chars": 20000
}
```

### 3) Read all regex matches with context: `rg`

This is the “grep -B/-A/-C, but bounded and merge-aware” tool for agents:

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
  "max_chars": 20000,
  // Optional: "numbered" (default) prefixes each line with "<line>: " and marks match lines as "<line>:* "
  // "format": "numbered"
}
```

### 4) Pagination (cursor)

When a tool response includes `truncated: true` and `next_cursor`, continue with a cursor call.

- Most tools require the original options (cursor is bound to them).
- `cat` and `rg` support cursor-only continuation (cursor carries the needed options), so the follow-up call can be just `{ "path": "...", "cursor": "<next_cursor>" }`.

For `read_pack`, take `next_cursor` (top-level) and continue with a cursor-only call (defaults keep output low-noise; `response_mode: "full"` can include per-section cursors when applicable):

```jsonc
{
  "path": "/path/to/project",
  "cursor": "<next_cursor>"
}
```

### 5) Batch v2 ($ref dependencies): chain tools in one call

Batch `version: 2` lets item inputs reference previous item outputs via JSON Pointer `$ref` (with optional `$default` fallback):

```jsonc
{
  "version": 2,
  "path": "/path/to/project",
  "max_chars": 20000,
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

Notes:

- `path` is canonical; `project` is accepted as an alias for consistency with the Command API.
- `action/payload` are accepted as aliases for `tool/input` (canonical) to mirror Command API batch.
- `$ref` must point to an earlier item’s `data` (JSON Pointer like `#/items/<id>/data/...`).
- Batch `version: 2` requires unique `items[].id`.
- `$ref` to a failed item is rejected; wrap with `$default` when you want a fallback value.
