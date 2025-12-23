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

Notes:
- The outer `options` apply to all items (freshness policy, budgets, filters).
- Item results are independent (`status: ok|error`); the batch itself can still be `ok` (partial success).

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
