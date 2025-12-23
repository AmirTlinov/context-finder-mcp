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

- One canonical JSON envelope (`{status,hints,data,meta}`) for programmatic use.
- `context-pack` is the default: bounded output under a hard `max_chars` budget.
- Hints and health signals exist to support agent decision-making, not just debugging.

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
- health snapshots for indexing (`/health`, `.context-finder/health.json`).

### 5) Offline-first, explicit dependencies

Models are **assets**:

- installed once (`install-models`) from a manifest,
- verified (sha256),
- and never committed to git.

### 6) No silent fallback

Reliability includes *performance predictability*.

- CUDA is the default execution policy.
- CPU fallback is opt-in (`CONTEXT_FINDER_ALLOW_CPU=1`).
- Tests should be deterministic and model-free (`CONTEXT_FINDER_EMBEDDING_MODE=stub`).

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

