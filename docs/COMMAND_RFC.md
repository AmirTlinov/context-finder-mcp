# Command API RFC

## 1. Goals

- One CLI entry point for programmatic use: `context-finder command --json '{...}'`.
- A single JSON response envelope: `{status,hints,data,meta}`.
- Reduce cognitive load: CLI subcommands build `CommandRequest` and reuse the same handler (or compose multiple requests).

## 1.1 Source of truth (contracts)

This RFC is human-oriented. The **canonical contracts** are machine-readable:

- Request schema: [contracts/command/v1/command_request.schema.json](../contracts/command/v1/command_request.schema.json)
- Response schema: [contracts/command/v1/command_response.schema.json](../contracts/command/v1/command_response.schema.json)
- HTTP surface: [contracts/http/v1/openapi.json](../contracts/http/v1/openapi.json) (`POST /command`, `GET /health`)

## 2. Request shape

```jsonc
{
  "action": "search",          // snake_case action identifier
  "payload": { ... },          // action-specific object
  "options": { ... } | null,   // cross-cutting options: freshness policy, filters, budgets
  "config": { ... } | null     // optional per-request overrides (merged into project config)
}
```

Notes:

- `action` names are snake_case (see `crates/cli/src/command/domain.rs`).
- Full request schema is defined in [contracts/command/v1/command_request.schema.json](../contracts/command/v1/command_request.schema.json).
- Unknown fields in `payload` are ignored unless the corresponding payload struct opts into stricter parsing.

### Actions overview

| action                | payload struct                | data struct                |
|----------------------|-------------------------------|----------------------------|
| `batch`              | `BatchPayload`                | `BatchOutput`              |
| `search`             | `SearchPayload`               | `SearchOutput`             |
| `search_with_context`| `SearchWithContextPayload`    | `SearchOutput`             |
| `context_pack`       | `ContextPackPayload`          | `ContextPackOutput`        |
| `meaning_pack`       | `MeaningPackPayload`          | `MeaningPackOutput`        |
| `meaning_focus`      | `MeaningFocusPayload`         | `MeaningPackOutput`        |
| `task_pack`          | `TaskPackPayload`             | `TaskPackOutput`           |
| `text_search`        | `TextSearchPayload`           | `TextSearchOutput`         |
| `evidence_fetch`     | `EvidenceFetchPayload`        | `EvidenceFetchOutput`      |
| `compare_search`     | `CompareSearchPayload`        | `ComparisonOutput`         |
| `index`              | `IndexPayload`                | `IndexResponse`            |
| `get_context`        | `GetContextPayload`           | `ContextOutput`            |
| `list_symbols`       | `ListSymbolsPayload`          | `SymbolsOutput`            |
| `config_read`        | `ConfigReadPayload`           | `ConfigReadResponse`       |
| `map`                | `MapPayload`                  | `MapOutput`                |
| `eval`               | `EvalPayload`                 | `EvalOutput`               |
| `eval_compare`       | `EvalComparePayload`          | `EvalCompareOutput`        |

All responses (including errors) include `meta.index_state` when the project root is resolvable,
providing a best-effort freshness snapshot (schema: [contracts/command/v1/index_state.schema.json](../contracts/command/v1/index_state.schema.json)).

### `batch` (one request → many actions)

Goal: enable an agent to do *one round-trip* and receive a **single bounded result**.

Source of truth:

- Batch schema: [contracts/command/v1/batch.schema.json](../contracts/command/v1/batch.schema.json)

Shape (simplified; see schema for canonical fields):

```jsonc
{
  "action": "batch",
  "options": { "stale_policy": "auto", "max_reindex_ms": 1500 },
  "payload": {
    "project": ".",
    "max_chars": 20000,
    "stop_on_error": false,
    "items": [
      { "id": "index", "action": "index", "payload": { "path": "." } },
      { "id": "pack", "action": "task_pack", "payload": { "intent": "find the indexing pipeline" } }
    ]
  }
}
```

Semantics:

- Batch items are processed sequentially.
- **Partial success:** the outer `CommandResponse.status` is `ok` if the batch request itself is valid; each item carries its own `status`.
- **No nested batch:** items cannot use `action=batch`.
- **Stable item ids:** `items[].id` is trimmed and must be unique within the batch.
- **Project consistency:** `payload.project` (or the first item project/path) becomes the batch project; items must not disagree.
- **Freshness guard is lazy:** `options.stale_policy` is enforced only right before the first item that requires an index (so `index → pack` is possible within one batch even with strict policies).
- `payload.max_chars` is a best-effort budget for the *serialized batch output*. When exceeded, the batch is truncated and the response carries a warning hint.

#### Ref dependencies (`$ref`)

Batch item payloads support lightweight dependencies via `$ref` wrappers (id-based JSON Pointer into *prior* item results):

```jsonc
{
  "id": "ctx",
  "action": "get_context",
  "payload": {
    "file": { "$ref": "#/items/search/data/matches/0/file" },
    "line": { "$ref": "#/items/search/data/matches/0/line" },
    "window": 20
  }
}
```

Notes:

- `$ref` is recognized only when the object contains exactly `$ref` (+ optional `$default`).
- `$ref` pointers are resolved against an evaluation context keyed by item `id` (so `#/items/<id>/...`, not `#/items/<index>/...`).
- `$ref` to a failed item’s `data` is rejected (use `$default` when you want a fallback).
- The MCP server `batch` tool uses the same `$ref` wrapper resolver in **batch v2** (canonical fields `tool/input`; `action/payload` are accepted as aliases to mirror Command API). The response layout is also aligned on `items[].id` so the same `#/items/<id>/...` pointers work across surfaces.

### Request options (cross-cutting)

`options` is shared across actions. Canonical schema:

- [contracts/command/v1/request_options.schema.json](../contracts/command/v1/request_options.schema.json)

High-signal knobs:

- `stale_policy`: `auto|warn|fail`
  - `auto`: best-effort incremental reindex within `max_reindex_ms` (no silent work: `meta.index_state.reindex` is filled).
  - `warn`: do not reindex; proceed with stale index and emit `warn` hints.
  - `fail`: do not reindex; return `error` if index is stale/missing.
- `max_reindex_ms`: time budget for `stale_policy=auto`.
- `include_paths` / `exclude_paths` / `file_pattern`: path filters for pack-like actions (`context_pack`, `task_pack`, `text_search`).
- `allow_filesystem_fallback`: controls whether `text_search` is allowed to scan files when no corpus exists.

## 3. Response shape

```jsonc
{
  "status": "ok" | "error",
  "message": "optional human text",
  "error": {
    "code": "machine_readable_code",
    "message": "human message",
    "details": { ... } | null,
    "hint": "optional hint",
    "next_actions": [{ "tool": "...", "args": { ... }, "reason": "..." }]
  },
  "hints": [
    { "type": "info" | "cache" | "action" | "warn" | "deprecation", "text": "..." }
  ],
  "next_actions": [
    { "tool": "...", "args": { ... }, "reason": "..." }
  ],
  "data": { ... },  // action-specific result
  "meta": { "index_state": { ... } | null, ... }   // diagnostics (see contract)
}
```

Canonical response contract:

- [contracts/command/v1/command_response.schema.json](../contracts/command/v1/command_response.schema.json)
- [contracts/command/v1/error.schema.json](../contracts/command/v1/error.schema.json)
- [contracts/command/v1/next_action.schema.json](../contracts/command/v1/next_action.schema.json)

Error interpretation:

- `error.next_actions` is always present (may be empty). When recovery is obvious (missing index, budget too small), it includes a ready-to-run retry action with tuned arguments.

Interpretation highlights:

- `index_state`: watermarks + staleness assessment + reindex attempt metadata (contract-first, stable).
- `graph_cache` + `graph_nodes`/`graph_edges`: whether the graph cache was used and how large the assembled graph is.
- `index_mtime_ms`: last index timestamp (unix-ms). Useful to detect stale results.
- `health_*`: watcher/index health signals and recent failures.
- `compare_*`: aggregated A/B metrics emitted by `compare_search`.

### Capabilities handshake

- `action: "capabilities"` returns server versions, default budgets, and a recommended starting tool call.
- Data schema: [contracts/command/v1/capabilities.schema.json](../contracts/command/v1/capabilities.schema.json)

## 4. CLI behavior

- `context-finder command --json '<request>'` prints the JSON `CommandResponse`.
- `context-finder serve-http --bind 127.0.0.1:7700` serves `POST /command` with the same request/response shape.
- Subcommands like `search/index/context-pack/...` build a `CommandRequest` and reuse the same handler (or compose multiple requests).
- Exit code is `0` if `status == "ok"`, otherwise `1`.

### Fast A/B metrics (`compare_search`)

- `compare_search` caches results by a hash of: queries + limit + strategy + reuse_graph + show_graph + language + index mtime.
- `invalidate_cache=true` forces recomputation for the current run.
- `data.summary` contains avg baseline/context latency, overlap ratio, and avg number of related chunks.
- `meta.index_size_bytes` / `meta.graph_cache_size_bytes` are quick signals for "warm" vs "cold" runs without external timing.
- `meta.warm` / `meta.warm_cost_ms` / `meta.warm_graph_cache_hit` describe process warmup (embedding + graph cache).
- `meta.timing_*` breaks down time into index load / graph build / search.
- `meta.duplicates_dropped` shows how many overlapping results were removed during merging.
- Task-aware hints are emitted automatically into `hints` (e.g. debug vs refactor vs perf scenarios).

#### Minimal A/B scenario

```bash
context-finder command --json '{
  "action": "compare_search",
  "payload": {
    "queries": ["search_with_context latency", "health snapshot"],
    "limit": 6
  }
}'
```

Expected:

- `status: "ok"`.
- `hints` contains a summary, plus cache-hit info when applicable.
- `data.summary.avg_baseline_ms` and `avg_context_ms` are comparable run-to-run.
- For "clean" measurements add `"invalidate_cache": true`.

### Interpreting `related`

- `results[].related[]` are additional code chunks pulled in via the code graph (calls, imports, tests, etc.).
- `relationship` is an edge-chain label (e.g. `Calls → Uses`), and `distance` is traversal depth.
- When `show_graph = true`, `results[].graph` includes `caller → callee` style edges for visualization.

## 5. Migration plan (historical)

1. Add the `command` entry point and implement the shared handler.
2. Rewrite subcommands as aliases and emit `deprecation` hints where needed.
3. Document that the JSON structure is stable (only the envelope adds `status/hints/meta`).
4. Later, mark legacy subcommands as deprecated.
