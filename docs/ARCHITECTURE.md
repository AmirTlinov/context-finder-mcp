# Architecture: Context Finder

## Workspace layout

Context Finder is implemented as a Rust workspace with a small set of focused crates:

```
context-finder/
├── crates/
│   ├── batch-ref/         # Batch $ref resolver (shared CLI/MCP)
│   ├── code-chunker/      # AST-aware semantic chunking (tree-sitter)
│   ├── vector-store/      # Embeddings + HNSW vector index (ONNX Runtime)
│   ├── indexer/           # Project scanning + incremental indexing
│   ├── search/            # Hybrid retrieval (semantic + fuzzy + fusion + rerank)
│   ├── graph/             # Code relationship graph (calls/uses/tests/...)
│   ├── cli/               # CLI + HTTP/gRPC servers + background daemon
│   └── mcp-server/        # MCP server for AI-agent integration
├── docs/                  # Documentation
├── profiles/              # Search heuristic profiles
└── Cargo.toml             # Workspace configuration
```

## Data flow

### 1) Indexing

```
   File System
        │
        ├─► Git repository scan (.gitignore-aware)
        │
        ▼
   ┌─────────────────┐
   │  File Scanner   │ ──► parallel file reading (tokio tasks)
   └────────┬────────┘
            │
            ▼
   ┌──────────────────┐
   │  Code Chunker    │  (tree-sitter)
   ├──────────────────┤
   │ • parse AST      │
   │ • extract symbols│
   │ • add context    │
   │ • compute meta   │
   └────────┬─────────┘
            │
            ├─────────────────┬─────────────────┬──────────────────┐
            ▼                 ▼                 ▼                  ▼
   ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐
   │  Corpus      │  │ Vector Store │  │ Fuzzy Index  │  │  Code Graph  │
   │ (chunks+meta)│  │ (HNSW)       │  │ (nucleo)     │  │ (relations)  │
   └──────────────┘  └──────────────┘  └──────────────┘  └──────────────┘
            │                 │                 │                  │
            └─────────────────┴─────────────────┴──────────────────┘
                              │
                              ▼
                      Persist to disk
           `.agents/mcp/context/.context/` (preferred)
```

### 2) Querying

```
   User query: "async error handling"
        │
        ▼
   ┌─────────────────────┐
   │  Query processing   │
   │ • tokenize/normalize│
   │ • classify intent   │
   └─────────┬───────────┘
             │
             ├──────────────────────┬──────────────────────┐
             ▼                      ▼                      ▼
    ┌────────────────┐    ┌──────────────────┐    ┌─────────────────┐
    │  Fuzzy search  │    │ Semantic search  │    │ Profile heuristics│
    │ (paths/text)   │    │ (embeddings)     │    │ (boosts/filters) │
    └───────┬────────┘    └────────┬─────────┘    └────────┬────────┘
            │                      │                        │
            └──────────┬───────────┴────────────────────────┘
                       │
                       ▼
              ┌─────────────────┐
              │ Fusion + rerank │
              │  (RRF + boosts) │
              └────────┬────────┘
                       │
                       ▼
            ┌────────────────────────┐
            │ Results / context pack │
            │ (primary + related)    │
            └────────────────────────┘
```

## Components

### Code Chunker (`crates/code-chunker`)

Responsibility: split source files into semantically meaningful chunks and enrich them with metadata used by retrieval and routing.

Chunking strategies (see `ChunkingStrategy`):

```rust
enum ChunkingStrategy {
    Semantic,     // AST boundaries (functions, classes, etc.)
    LineCount,    // fixed line count (fast)
    TokenAware,   // token-based, syntax-aware
    Hierarchical, // parent context + focused element
}
```

Presets (see `ChunkerConfig`):

- `ChunkerConfig::for_speed()`
- `ChunkerConfig::for_embeddings()`
- `ChunkerConfig::for_llm_context()`

### Vector Store (`crates/vector-store`)

Responsibility: embed chunks and build/load a vector index for fast semantic retrieval.

Key points:

- Embeddings are computed via ONNX Runtime (CUDA by default).
- CPU fallback is allowed only when `CONTEXT_FINDER_ALLOW_CPU=1`.
- Index is stored per model id under `.agents/mcp/context/.context/indexes/<model_id>/` (preferred; legacy `.context/` and `.context-finder/` are supported).

### Search (`crates/search`)

Responsibility: hybrid retrieval + fusion + reranking.

- Fuzzy search: fast path/content matching.
- Semantic search: embedding similarity (HNSW).
- Fusion: reciprocal rank fusion (RRF).
- Rerank: profile-driven boosts and thresholds.

Profiles (`profiles/*.json`) are the primary way to tune behavior (routing, boosts, must-hit rules, rerank thresholds, embedding templates).

### Graph (`crates/graph`)

Responsibility: build a code relationship graph (calls, uses, tests, imports, etc.) and support graph-aware context assembly.

Used by:

- `search_with_context` / `context` (attach related chunks)
- `context_pack` / `context-pack` (bounded output under a character budget)

### Indexer (`crates/indexer`)

Responsibility: scan projects, (re)build indexes, and support incremental updates.

Key points:

- `.gitignore`-aware scanning (crate `ignore`).
- Incremental rebuild via mtimes snapshot + file watcher.
- Persists a health snapshot to `.agents/mcp/context/.context/health.json`.

### CLI (`crates/cli`)

Responsibility: user-facing interface and service modes.

Notable commands:

- Search/index: `index`, `search`, `context`, `context-pack`, `map`, `list-symbols`
- Ops: `install-models`, `doctor`
- Evaluation: `eval`, `eval-compare`
- JSON API: `command`, `serve-http`, `serve-grpc`
- Daemon: `daemon-loop` (keeps indexes warm)

### MCP server (`crates/mcp-server`)

Responsibility: expose Context Finder capabilities as MCP tools for AI-agent integrations.

Transport:

- stdio JSON-RPC with `Content-Length` framing

For the tool list and examples, see `README.md`.

## On-disk layout (per project)

```
.agents/mcp/context/.context/   # preferred (legacy: .context/, .context-finder/)
├── corpus.json                     # chunk corpus (text + metadata)
├── indexes/
│   └── <model_id>/
│       ├── index.json              # vector store index
│       ├── meta.json               # store metadata (mode/templates/dimension)
│       └── mtimes.json             # incremental mtimes snapshot
├── graph_cache.json                # cached code graph (optional)
├── health.json                     # indexer health snapshot
├── config.json                     # per-project config (optional)
├── profiles/                       # per-project profiles (optional)
└── cache/                          # compare_search and heavy-op caches
```

## Configuration

### Runtime defaults

- Models: installed into `./models/` by default (`models/manifest.json` is the source of truth).
- GPU: CUDA by default; no silent CPU fallback.
- Deterministic tests: `CONTEXT_FINDER_EMBEDDING_MODE=stub`.

### Tuning knobs

The project prefers "configuration at the edges" over hard-coded constants:

- Chunking behavior: `ChunkerConfig` (see `crates/code-chunker/src/config.rs`).
- Retrieval/rerank behavior: `SearchProfile` in `profiles/*.json` (see `profiles/general.json` for a full example).
- Per-project overrides: `.agents/mcp/context/.context/config.json` and `.agents/mcp/context/.context/profiles/` (legacy `.context/` / `.context-finder/` still work).

## Algorithms

### Reciprocal Rank Fusion (RRF)

RRF merges multiple ranked lists without requiring score normalization.

```
Inputs: rankings R1..Rm, parameter k (commonly 60)

For each document d:
  score(d) = Σ(i=1..m) 1 / (k + rank_i(d))

Output: documents sorted by descending score(d)
```

### HNSW (Hierarchical Navigable Small World)

HNSW is an approximate nearest neighbor (ANN) structure:

- Index build inserts vectors into a layered small-world graph.
- Query starts at the top layer and greedily descends to refine the candidate set.
- Final top-k comes from the bottom layer neighborhood search.

### Graph-based context assembly

Graph-aware modes expand primary hits with "related" chunks:

- Calls and callees
- Imports and dependencies
- Tests that exercise a symbol

`context-pack` bounds the output size via a character budget (`max_chars`) and caps the per-primary halo (`max_related_per_primary`).

## Limitations and trade-offs

| Aspect | Trade-off | Mitigation |
|--------|-----------|------------|
| Memory | vector index + corpus can be large | shard via multiple models/profiles; prefer incremental indexing |
| Cold start | initial index build cost | keep `.agents/mcp/context/.context/` cached; use daemon-loop |
| Language support | depends on tree-sitter grammars | fall back to non-AST modes where needed |
| Real-time updates | watcher has debounce/latency | acceptable for dev; run `index --force` when needed |
