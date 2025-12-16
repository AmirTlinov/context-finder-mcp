#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${ROOT_DIR}"

CLI="${CLI:-./target/release/context-finder}"
if [[ ! -x "${CLI}" ]]; then
  echo "[test_integration] CLI not found at ${CLI}. Build it with: cargo build --release -p context-finder-cli" >&2
  exit 1
fi

EMBED_MODE="${CONTEXT_FINDER_EMBEDDING_MODE:-stub}"
COMMON=(--quiet --embed-mode "${EMBED_MODE}")

echo "=== INTEGRATION TESTING CONTEXT-FINDER ==="
echo ""

echo "Indexing (required for search/map)..."
"${CLI}" "${COMMON[@]}" index . --json >/dev/null
echo "OK"
echo ""

echo "TEST 1: Search - 'error handling'"
"${CLI}" "${COMMON[@]}" search "error handling" -n 5 --json \
  | jq -r '.data.results // [] | .[] | "\(.file):\(.start_line) - \(.symbol // "unknown") (score: \(.score))"'
echo ""

echo "TEST 2: Search - 'AST parsing'"
"${CLI}" "${COMMON[@]}" search "AST parsing" -n 5 --json \
  | jq -r '.data.results // [] | .[] | "\(.file):\(.start_line) - \(.symbol // "unknown") (score: \(.score))"'
echo ""

echo "TEST 3: Search - 'chunk_by_tokens'"
"${CLI}" "${COMMON[@]}" search "chunk_by_tokens" -n 5 --json \
  | jq -r '.data.results // [] | .[] | "\(.file):\(.start_line) - \(.symbol // "unknown") (score: \(.score))"'
echo ""

echo "TEST 4: Search typo - 'embeding' (should find 'embedding')"
"${CLI}" "${COMMON[@]}" search "embeding" -n 5 --json \
  | jq -r '.data.results // [] | .[] | "\(.file):\(.start_line) - \(.symbol // "unknown") (score: \(.score))"'
echo ""

echo "TEST 5: Search - 'vector similarity'"
"${CLI}" "${COMMON[@]}" search "vector similarity" -n 5 --json \
  | jq -r '.data.results // [] | .[] | "\(.file):\(.start_line) - \(.symbol // "unknown") (score: \(.score))"'
echo ""

echo "TEST 6: Search - 'chunk code into functions'"
"${CLI}" "${COMMON[@]}" search "chunk code into functions" -n 5 --json \
  | jq -r '.data.results // [] | .[] | "\(.file):\(.start_line) - \(.symbol // "unknown") (score: \(.score))"'
echo ""

echo "TEST 7: Search - 'main' (should find main function)"
"${CLI}" "${COMMON[@]}" search "main" -n 5 --json \
  | jq -r '.data.results // [] | .[] | "\(.file):\(.start_line) - \(.symbol // "unknown") (score: \(.score))"'
echo ""

echo "TEST 8: List symbols in chunker.rs"
"${CLI}" "${COMMON[@]}" list-symbols . --file crates/code-chunker/src/chunker.rs --json \
  | jq -r '.data.symbols[]? | "\(.line): \(.type) \(.name) (parent: \(.parent // "none"))"'
echo ""

echo "TEST 9: Get aggregated context for queries"
"${CLI}" "${COMMON[@]}" get-context "chunk_by_tokens" "embedding mode" --path . -n 3 --json \
  | jq 'if length > 0 then {count: length, top: .[0] | {file, start_line, symbol, score}} else {count: 0} end'
echo ""

echo "=== TESTING COMPLETE ==="
