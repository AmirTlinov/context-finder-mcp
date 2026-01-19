# Agent Memory Playbook (`read_pack`)

Context Finder is meant to feel like an AI agent’s **project memory**: one call produces a bounded, high-signal view of the repo; you continue with `cursor` when needed; and you do not burn the agent’s context window on tool chatter.

If you still need `rg → open → grep → cat → repeat`, treat that as a product bug or a missing workflow pattern.

## Mental model (how to think about it)

`read_pack` always starts with:

1) `project_facts` — stable, compact “map of the world” (ecosystems, build tools, CI, contracts, key dirs, entry points, key configs)
2) payload sections — snippets that contain *project content* (docs/config/code), bounded by `max_chars`
   - For long docs/configs, the “memory pack” tries to **anchor** snippets around high-signal, stable lines (tests/run/config headings) instead of always starting from the top.
   - High-signal configs include CI/devcontainer and editor task configs (e.g. `.github/workflows/*`, `.devcontainer/devcontainer.json`, `.vscode/tasks.json`)
   - If the repo doesn’t follow common naming (`README`, `AGENTS`, etc.), the memory pack uses a bounded fallback to discover a few doc-like files from common doc roots (root, `docs/`, etc) before giving up.
2.5) optional `external_memory` — bounded, deduped “worklog memory” if available
   - `codex_cli`: project-scoped prompts/plans/changes extracted from your Codex CLI sessions (zero-config)
   - `branchmind`: structured decisions/evidence overlay when a project-scoped cache file is present
3) `next_cursor` (when needed) — continue without re-sending parameters

### Codex CLI overlay (`codex_cli`)

`codex_cli` is a zero-config overlay: it reads your Codex CLI session transcripts under
`$CODEX_HOME/sessions/**/rollout-*.jsonl` and extracts bounded, project-scoped “worklog memory”.

To make the overlay reliably high-signal, prefer explicit tags in assistant replies:

- `[decision] ...`
- `[plan] ...`
- `[evidence] ...`
- `[blocker] ...`
- `[change] ...`

The extractor also recognizes common heading-style formats in both RU/EN (e.g. `РЕШЕНИЕ:`,
`ДАЛЬШЕ:`, `ДОКАЗАТЕЛЬСТВА:`, `DECISION:`, `NEXT:`, `EVIDENCE:`), plus a best-effort implicit
fallback — but explicit tags are the most deterministic.

Dedup & merge behavior is intentionally “agent-native”:

- `decision` and `plan` hits are deduped by a semantic key to avoid repeated copies across
  messages.
- Decisions are merged into a bounded superset so summary-only followups don’t delete older
  technical details.
- Plans collapse duplicates even when statuses change; the newest plan text is treated as
  “truth” while the semantic key ignores status churn.

Ingestion is truly incremental:

- A per-session byte cursor is persisted so each refresh reads only newly appended JSONL lines
  instead of re-scanning/tailing long sessions.
- A partially-written trailing JSON event is not acknowledged (cursor not advanced past the last
  newline boundary) until it parses as JSON, preventing event loss.

The default `response_mode` is designed to be low-noise:

- `"facts"`: payload-first; strips helper guidance and avoids heavy diagnostics by default
- `"minimal"`: smallest possible output (max context savings)
- `"full"`: opt-in “rich” output (diagnostics, freshness details) when you explicitly want it

For tight-loop navigation/read tools (`cat`, `rg`, `ls`, `text_search`, `tree`), the default is typically `"minimal"` so answers are mostly *project content*.

Note: In low-noise modes, `rg` defaults to `format: "plain"` (higher payload density). Set `format: "numbered"` if you want per-line number prefixes in the hunk content.

Note: `rg` / `ls` / `text_search` treat the filesystem as the source of truth even when a corpus exists — corpora can be partial (scoped indexing, in-progress indexing), and tight-loop read tools must never silently miss files.

Note: `text_search` is budget-first. Use `max_chars` to bound the response size; under tight budgets it returns a small number of matches (possibly with truncated match text) plus `next_cursor` for continuation.

Note: `tree` (legacy: `map`) is primarily an onboarding/overview helper. In `"minimal"` it returns mostly directory paths (lowest noise); in `"full"` it can include richer diagnostics (e.g. top symbols / coverage).

## Output format (MCP): `.context` text (agent-native)

The Command API (CLI/HTTP/gRPC) returns structured JSON payloads (best for strict programmatic parsing and contracts).

For MCP tools, the agent-facing `content` is always a packed `.context` **plain text** document: a `[CONTENT]` stream. The legend (`[LEGEND]`) is available via the `help` tool when you need it.

The goal is payload density: responses should be **almost entirely project content** (facts + snippets), with minimal tool chatter.

MCP tool output is intentionally `.context` text only (agent-native). For machine-readable output (automation / batching / `$ref` workflows), use the Command API.

To keep agent context windows clean, `[LEGEND]` is **not** included in regular tool output (even in `"full"`). Use the `help` tool to fetch the legend on demand.

## Secrets (safe by default)

Agent context windows are “sticky” — once a secret is printed, it tends to spread. Context Finder therefore uses a conservative denylist by default:

- `cat` refuses to read common secret locations (e.g. `.env`, SSH keys, `*.pem`/`*.key`) (legacy: `file_slice`)
- `rg` / `text_search` skip secret paths (and refuse explicit secret file reads) (legacy: `grep_context`)
- `read_pack` refuses `intent=file` reads of secrets, and skips secrets in filesystem fallbacks

If you *explicitly* need to inspect secret files (debugging local env, investigating credentials leakage, etc.), you can opt in per call:

```jsonc
{ "allow_secrets": true }
```

This flag is captured in cursors, so cursor-only continuation stays consistent and safe.

## Day 0: first call in any repo

Use the default call to get stable facts + high-signal docs/config snippets:

```jsonc
{ "path": "/path/to/project" }
```

You can pass either a project directory or **any file path inside the project** (agent-native: the server will treat a file path as a “root hint” and use its parent directory):

```jsonc
{ "path": "/path/to/project/src/main.rs" }
```

When `path` is a **file path hint**, `read_pack intent=memory` will also try to surface a bounded
snippet from that file on the **first memory page** (no cursor). This makes the memory pack feel
more like a “native working set” (stable repo anchors + the current file) without requiring an
explicit `intent=file` call.

If you are running Context Finder MCP from *inside* the repo you care about (typical agent session),
you can omit `path` entirely — the shared-backend proxy will inject a default root from its current
working directory (preferring the git root when available):

```jsonc
{}
```

If the repo is large and you want a more guided onboarding experience (default `max_chars` is small; cursor-first continuation is expected):

```jsonc
{ "path": "/path/to/project", "intent": "onboarding" }
```

## Day N: ask multiple questions in one call (recall)

Recall mode is the fastest way to “remember what matters” without many calls:

```jsonc
{
  "path": "/path/to/project",
  "max_chars": 6000,
  "questions": [
    "Where are the main entry points and routes?",
    "How do I run tests and lint?",
    "Where is configuration loaded from?",
    "deep index:8s in:src k:5 How does the main request flow work?"
  ]
}
```

Budget tip: recall tries to answer multiple questions per call under the shared `max_chars` cap. As a rough rule of thumb, `max_chars ≈ 6000` usually fits ~2 questions; larger budgets can fit more (up to a small safety cap) before switching to cursor pagination.

### “Structural” questions (deterministic routing)

For common “repo memory” asks like:

- “Where are the main entry points / binaries?”
- “Where is the protocol/contract documented?”
- “Where is configuration stored / loaded from?”

Recall mode uses a deterministic, doc-first candidate set (README / architecture / contract docs / key configs / entrypoint files) before falling back to semantic search. This is intentional: it keeps answers stable, high-signal, and avoids “random code snippets” that look relevant but are not the repo’s actual integration surface.

### Recall per-question mini-language (optional)

Each string in `questions[]` may include small directives:

- Routing: `fast` (file/grep-first), `deep` (semantic allowed; can auto-index)
- Scoping: `in:<prefix>`, `not:<prefix>`
- File filter: `glob:<pattern>` / `fp:<pattern>`
- File jump: `file:<path[:line]>` / `open:<path[:line]>`
- Grep intent: `re:<regex>` / `lit:<text>`
- Output control: `k:<N>` (snippets per question), `ctx:<N>` (grep context lines)
- Deep indexing budget: `index:5s`, `deep:8000ms`

These directives are not part of the schema: they are parsed from the question text to keep the public contract minimal.

Note on freshness: `deep` is treated as an explicit opt-in to semantic work and will enable auto-indexing by default. If you set `auto_index: false` explicitly and the semantic index is stale, `read_pack` will deterministically fall back to filesystem strategies instead of returning silently stale semantic results.

## Continuation (cursor)

Continuation is always cursor-first:

- In the `.context` text output, look for a line like `M: <cursor>` near the end.
- In Command API batch workflows, look for `next_cursor` in the per-item result payload.

Continue with a cursor-only call:

```jsonc
{ "cursor": "<cursor>" }
```

If a cursor is present (`next_cursor` in Command API JSON, or `M:` in `.context`), treat the response as incomplete even when it still fits under `max_chars`: it is the one reliable “there is more” signal.

Cursor tokens now embed the project root (and any relevant options), so you don’t have to resend `path` for pagination — even if your first call targeted a non-default project directory.

For tight-loop read tools (`cat`, `rg`, `text_search`), cursor-only continuation also works directly on those tools — options are captured in the cursor so you don’t have to retype them.

Safety note (multi-agent): if a session already has a default project root, tools refuse to switch projects based on a cursor token alone. To switch roots intentionally, pass an explicit `path`.

Note: you can override `max_chars` between pages if you want to change the payload density (smaller budget for “peek”, larger budget for “read more”). If omitted, the tool reuses the cursor’s previous `max_chars`.

In `response_mode: "full"`, some sections/snippets can include their own cursor hint (for continuing a file window). By default (`facts`/`minimal`), `read_pack` avoids embedding per-snippet cursors to keep the response mostly project content.

The “memory pack” itself can also paginate: if there are more high-signal candidates than fit in the current `max_chars`, `read_pack` emits a cursor to continue the next page of memory snippets.

Note: Some cursors may be compact and backed by short-lived server-side continuation state (to avoid cursor bloat). If a continuation expires (or the MCP process restarts), repeat the original call that produced it.

In shared-backend mode (the default), cursor aliases are stored in the long-lived daemon and persisted best-effort on disk, so short `cfcs2:…` cursors typically survive process restarts as long as their TTL has not expired (`cfcs1:…` is legacy).

## Multi-agent: shared MCP backend (default)

If you run many agent sessions in parallel, starting a full MCP server process per session is wasteful and makes cursor-only continuation fragile (because the server-side cursor store may be lost on restart).

Context Finder uses a shared backend mode by default:

- Each agent session starts a lightweight stdio proxy.
- All proxies connect to **one** long-lived MCP daemon process.
- The daemon keeps the **engine cache** and **cursor store** warm across sessions, which makes the tool feel closer to a “project memory” rather than a disposable command.
- Session defaults (like omitting `path` after your first call) are **connection-local**, so parallel agent sessions won’t accidentally “steal” each other’s default project root.

If you need an isolated in-process MCP server per session (mostly useful in tests), disable shared mode:

```text
CONTEXT_FINDER_MCP_SHARED=0
```

Optional:

- `CONTEXT_FINDER_MCP_SOCKET` to override the Unix socket path.
- Keep the indexing daemon enabled (do **not** set `CONTEXT_FINDER_DISABLE_DAEMON=1`) if you want indexes to stay warm while you work.

## What to avoid (anti-patterns)

- Doing 10 separate calls to “locate files → grep → open → grep again”.
- Always requesting `"full"`: it increases overhead and reduces payload under tight budgets.
- Asking for a giant `max_chars` by default: prefer a smaller budget + cursor continuation.

## Source of truth (contracts)

Treat the MCP schema as the canonical reference for exact fields:

- `read_pack`: `crates/mcp-server/src/tools/schemas/read_pack.rs`
- `cat` (legacy: `file_slice`): `crates/mcp-server/src/tools/schemas/file_slice.rs`
- `rg` (legacy: `grep_context`): `crates/mcp-server/src/tools/schemas/grep_context.rs`
