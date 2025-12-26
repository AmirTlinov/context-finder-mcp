# Context Finder — Usage Examples (agent-first)

This document focuses on agent-friendly workflows: index once, then request bounded context in a single call.

## Quick start

### 1) Build/install

```bash
cargo build --release
cargo install --path crates/cli

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
context-finder list-symbols . --file "crates/cli/src/main.rs" --json --quiet
```

## Quality evaluation (golden datasets)

```bash
# Evaluate MRR/recall/latency/bytes + artifacts
context-finder eval . --dataset datasets/golden_smoke.json --cache-mode warm \
  --out-json .context-finder/eval.smoke.json \
  --out-md .context-finder/eval.smoke.md \
  --json

# A/B comparison across profiles/model sets
context-finder eval-compare . --dataset datasets/golden_smoke.json \
  --a-profile general --b-profile general \
  --a-models bge-small --b-models embeddinggemma-300m \
  --out-json .context-finder/eval.compare.json \
  --out-md .context-finder/eval.compare.md \
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

Use this as the default first step when dropped into a new repository:

```jsonc
{
  "path": "/path/to/project",
  "map_depth": 2,
  "docs_limit": 6,
  "max_chars": 20000
}
```

### 2) One-call reading pack (file/grep/query): `read_pack`

Use `read_pack` when you want a single entry point and cursor-only continuation.
It returns `sections[]` where each section has `{ "type": "...", "result": { ... } }` (the underlying tool output).

Read a file window (internally calls `file_slice`):

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

Read all regex matches with N lines of context (internally calls `grep_context`):

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

### 3) Read all regex matches with context: `grep_context`

This is the “grep -B/-A/-C, but bounded and merge-aware” tool for agents:

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

### 4) Pagination (cursor)

When a tool response includes `truncated: true` and `next_cursor`, continue by repeating the call with the same options + `cursor: "<next_cursor>"`.

This works for: `map`, `list_files`, `text_search`, `grep_context`, `file_slice`.

For `read_pack`, take the cursor from `sections[0].result.next_cursor` and continue with a cursor-only call:

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

- `path` is canonical; `project` is accepted as an alias for consistency with the Command API.
- `action/payload` are accepted as aliases for `tool/input` (canonical) to mirror Command API batch.
- `$ref` must point to an earlier item’s `data` (JSON Pointer like `#/items/<id>/data/...`).
- Batch `version: 2` requires unique `items[].id`.
- `$ref` to a failed item is rejected; wrap with `$default` when you want a fallback value.
