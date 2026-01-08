# Philosophy

Context Finder exists to make AI-assisted code work **predictable**:
one query in, one bounded context out — with interfaces you can trust and automate against.

## The principles

### 1) Contract-first, always

If it crosses a process boundary, it is a **contract**.

- Contracts are machine-readable (`contracts/…`, `proto/…`).
- Prose docs explain and link back — they are not the source of truth.
- Breaking changes require a new compatibility line (`vN → v(N+1)`).

### 2) Agent-first ergonomics

Humans can click around. Agents need stable shapes.

- Stable, machine-readable contracts for every surface (CLI JSON, HTTP, gRPC, MCP).
- A single daily “project memory” entrypoint: `read_pack` (MCP) is the “apply_patch of context”.
- Bounded outputs everywhere: hard budgets (`max_chars`) + deterministic truncation + cursor continuation.
- Tool “richness” is explicit and opt-in (`response_mode`), so tight-loop reads stay payload-dense.

Agent ergonomics here is not “tell the agent what to do” — it is **make the right thing the easiest thing**.
If an agent still feels the need to loop on `rg → open → grep → cat → repeat`, that is a product failure.

Practical target UX:

- **Project memory, not shell probing:** `read_pack` (MCP) is the “apply_patch of context” — one call returns stable `project_facts` + relevant snippets under one budget.
- **One warm brain, many hands:** the MCP server defaults to a shared backend daemon so cursor continuation and caches stay stable across sessions (set `CONTEXT_MCP_SHARED=0` only if you need an isolated per-session server).
- **Low-noise by default:** default outputs should be mostly *project content*, not tool meta (“semantic sugar” is opt-in).
- **Context-first output:** MCP tools return `.context` text only (agent-native). For machine-readable JSON, use the Command API (CLI/HTTP/gRPC).
- **Cursor-first continuation:** if it doesn’t fit, the next step is always `cursor`, not “retype parameters”.
- **Compact cursors:** continuation tokens should be cheap in the agent context window (short cursor aliases, server-backed when needed).
- **Cursor continuity:** in shared-backend mode, server-backed cursor aliases are persisted best-effort on disk so continuations typically survive process restarts (TTL-limited).
- **Secret-safe by default:** read tools refuse or skip common secret locations unless explicitly enabled (`allow_secrets=true`).
- **Freshness-safe fallback:** when semantic freshness is not guaranteed and auto-index is disabled, prefer deterministic filesystem strategies over silently stale semantic output.

### 3) Bounded outputs beat “smart” outputs

Most agent failures are not “bad ranking” — they are **too much text** or **unbounded IO**.

We bias the system toward:

- explicit budgets,
- deterministic truncation,
- traceable decisions (meta + hints),
- and a clean “one call → one pack” workflow.

### 4) Measured quality, not vibes

We prefer objective evaluation over anecdotes:

- golden datasets (`datasets/…`) with MRR/recall/latency/bytes,
- A/B comparisons (`eval-compare`, `compare_search`),
- health snapshots for indexing (`/health`, `.agents/mcp/context/.context/health.json`).

### 5) Offline-first, explicit dependencies

Models are **assets**:

- installed once (`install-models`) from a manifest,
- verified (sha256),
- and never committed to git.

### 6) No silent fallback

Reliability includes *performance predictability*.

- CUDA is the default execution policy.
- CPU fallback is opt-in (`CONTEXT_ALLOW_CPU=1`).
- Tests should be deterministic and model-free (`CONTEXT_EMBEDDING_MODE=stub`).

## What we optimize for

- Stable integration surfaces (contracts).
- Predictable cost and output size (budgets).
- Clear failure modes (health + hints).
- Easy-to-change internals without breaking users.

## What we don’t optimize for (non-goals)

- Being a general-purpose hosted search service.
- Perfect answers from a cold start without indexing.
- Infinite flexibility at the API boundary.

## Pointers

- Contracts: `contracts/README.md`
- Architecture: `docs/ARCHITECTURE.md`
- Command API overview: `docs/COMMAND_RFC.md`
- Context Pack schema: `docs/CONTEXT_PACK.md`
