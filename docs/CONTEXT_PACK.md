# Context Pack (v1)

`context-pack` is a bounded agent output: one query → one compact JSON with the most relevant code chunks.

The Command API (CLI/HTTP/gRPC JSON envelope) returns `ContextPackOutput` under `CommandResponse.data`.

The MCP tool `context_pack` returns an agent-facing `.context` text document (high signal, low noise).
For machine-readable workflows (automation / batching / `$ref` fan-out), use the Command API.

Canonical schema (source of truth):

- [contracts/command/v1/context_pack.schema.json](../contracts/command/v1/context_pack.schema.json)

## Filtering (recommended)

For agent workloads, reduce noise early by filtering paths at the request level:

- Command API: `options.include_paths` / `options.exclude_paths` / `options.file_pattern`
- MCP tool: `include_paths` / `exclude_paths` / `file_pattern`

Semantics:

- `include_paths` / `exclude_paths`: prefix match on relative paths
- `file_pattern`: substring match, or `glob` when it contains `*` / `?`

These filters are applied during pack assembly (so they affect `budget` deterministically).

## Code vs docs preference (agent ergonomics)

`context_pack` can be tuned to be implementation-first or documentation-first:

- `prefer_code` (bool): when `true`, rank code/test/config before markdown docs; when `false`, rank docs before code.
- `include_docs` (bool): when `false`, exclude `*.md` / `*.mdx` from both primary and related items.
- `related_mode` (`explore` | `focus`):
  - `explore` keeps a broader halo (good for research / onboarding).
  - `focus` gates related items by query-token hits and favors matching chunks (good for “where/how implemented”).

Defaults are chosen heuristically for agent workflows:

- For docs-intent queries (`README`, `docs`, `tutorial`, `*.md`), default is docs-first + `related_mode=explore`.
- For identifier/path queries, default is code-first + `related_mode=focus`.

## Schema (data)

```jsonc
{
  "version": 1,
  "query": "string",
  "model_id": "string",
  "profile": "string",
  "items": [
    {
      "id": "path:start_line:end_line",
      "role": "primary|related",
      "file": "path",
      "start_line": 1,
      "end_line": 2,
      "symbol": "optional string",
      "type": "optional string (chunk type)",
      "score": 0.0,
      "imports": ["..."],
      "content": "string",
      "relationship": ["optional edge labels..."], // optional
      "distance": 1                                // optional
    }
  ],
  "budget": {
    "max_chars": 20000,
    "used_chars": 1234,
    "truncated": false,
    "dropped_items": 0
  },
  "meta": {
    "index_state": { /* best-effort, see index_state.schema.json */ }
  }
}
```

## Index freshness metadata

`ContextPackOutput.meta.index_state` provides a best-effort snapshot of the current project
watermark and index freshness. It is included when the project root is resolvable; otherwise it
may be null or omitted by the caller.

- Canonical schema: [contracts/command/v1/index_state.schema.json](../contracts/command/v1/index_state.schema.json)

## Auto-index policy (MCP tool)

The MCP tool `context_pack` can auto-build or refresh the semantic index:

- `auto_index` (default: true)
- `auto_index_budget_ms` (default: 15000, clamped 100..120000)

When enabled, missing or stale indexes trigger a best-effort reindex before search. The outcome
is reported under `meta.index_state.reindex`. Set `auto_index=false` to fail fast when no index
is available.

For the Command API, use `options.stale_policy` and `options.max_reindex_ms` instead.

## Response mode (MCP tool)

For agent workflows that need the smallest possible payload (maximum signal per token),
the MCP tool supports a noise-reduction switch:

- `response_mode: "facts"` (default): keeps freshness `meta.index_state` but stays low-noise
- `response_mode: "full"`: includes `meta.index_state` plus extra diagnostics
- `response_mode: "minimal"`: strips `meta.index_state`; `trace` is ignored

## Examples

### 1) Identifier query

```json
{
  "budget": {
    "dropped_items": 1,
    "max_chars": 800,
    "truncated": true,
    "used_chars": 250
  },
  "items": [
    {
      "content": "use std::path::{Path, PathBuf}\n\npub(crate) struct EmbeddingCache {\n    base_dir: PathBuf,\n}",
      "end_line": 9,
      "file": "crates/vector-store/src/embedding_cache.rs",
      "id": "crates/vector-store/src/embedding_cache.rs:7:9",
      "imports": [
        "use std::path::{Path, PathBuf}"
      ],
      "role": "primary",
      "score": 1.0,
      "start_line": 7,
      "symbol": "EmbeddingCache",
      "type": "struct"
    }
  ],
  "model_id": "bge-small",
  "profile": "quality",
  "query": "EmbeddingCache",
  "version": 1
}
```

### 2) File-path query

```json
{
  "budget": {
    "dropped_items": 1,
    "max_chars": 800,
    "truncated": true,
    "used_chars": 531
  },
  "items": [
    {
      "content": "pub const CHUNK_CORPUS_SCHEMA_VERSION: u32 = 1;",
      "end_line": 7,
      "file": "crates/vector-store/src/corpus.rs",
      "id": "crates/vector-store/src/corpus.rs:7:7",
      "imports": [],
      "role": "primary",
      "score": 1.0,
      "start_line": 7,
      "symbol": "CHUNK_CORPUS_SCHEMA_VERSION",
      "type": "const"
    },
    {
      "content": "use context_code_chunker::CodeChunk\nuse std::collections::{BTreeMap, HashSet}\n\npub struct ChunkCorpus {\n    files: BTreeMap<String, Vec<CodeChunk>>,\n}",
      "end_line": 12,
      "file": "crates/vector-store/src/corpus.rs",
      "id": "crates/vector-store/src/corpus.rs:10:12",
      "imports": [
        "use context_code_chunker::CodeChunk",
        "use std::collections::{BTreeMap, HashSet}"
      ],
      "role": "primary",
      "score": 0.9990000128746033,
      "start_line": 10,
      "symbol": "ChunkCorpus",
      "type": "struct"
    }
  ],
  "model_id": "bge-small",
  "profile": "quality",
  "query": "crates/vector-store/src/corpus.rs",
  "version": 1
}
```
