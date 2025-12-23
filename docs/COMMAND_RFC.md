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
| `search`             | `SearchPayload`               | `SearchOutput`             |
| `search_with_context`| `SearchWithContextPayload`    | `SearchOutput`             |
| `context_pack`       | `ContextPackPayload`          | `ContextPackOutput`        |
| `compare_search`     | `CompareSearchPayload`        | `ComparisonOutput`         |
| `index`              | `IndexPayload`                | `IndexResponse`            |
| `get_context`        | `GetContextPayload`           | `ContextOutput`            |
| `list_symbols`       | `ListSymbolsPayload`          | `SymbolsOutput`            |
| `config_read`        | `ConfigReadPayload`           | `ConfigReadResponse`       |
| `map`                | `MapPayload`                  | `MapOutput`                |
| `eval`               | `EvalPayload`                 | `EvalOutput`               |
| `eval_compare`       | `EvalComparePayload`          | `EvalCompareOutput`        |

## 3. Response shape

```jsonc
{
  "status": "ok" | "error",
  "message": "optional human text",
  "hints": [
    { "type": "info" | "cache" | "action" | "warn" | "deprecation", "text": "..." }
  ],
  "data": { ... },  // action-specific result
  "meta": { ... }   // optional diagnostics (see contract)
}
```

Canonical response contract:

- [contracts/command/v1/command_response.schema.json](../contracts/command/v1/command_response.schema.json)

Interpretation highlights:

- `graph_cache` + `graph_nodes`/`graph_edges`: whether the graph cache was used and how large the assembled graph is.
- `index_mtime_ms`: last index timestamp (unix-ms). Useful to detect stale results.
- `health_*`: watcher/index health signals and recent failures.
- `compare_*`: aggregated A/B metrics emitted by `compare_search`.

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
