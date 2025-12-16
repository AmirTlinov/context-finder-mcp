#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${ROOT_DIR}"

CLI="${CLI:-./target/debug/context-finder}"
if [[ ! -x "${CLI}" ]]; then
  CLI="./target/release/context-finder"
fi
if [[ ! -x "${CLI}" ]]; then
  echo "[test_batch_search] CLI not found. Build it with: cargo build -p context-finder-cli" >&2
  exit 1
fi

EMBED_MODE="${CONTEXT_FINDER_EMBEDDING_MODE:-stub}"
COMMON=(--quiet --embed-mode "${EMBED_MODE}")

echo "=== Testing Multi-Query Search (get-context) ==="
echo

echo "1) Index (required for search)"
"${CLI}" "${COMMON[@]}" index . --json >/dev/null
echo "OK"
echo

echo "2) Sequential search (baseline)"
for query in "error handling" "embedding model" "fuzzy matching"; do
  echo "Query: '${query}'"
  "${CLI}" "${COMMON[@]}" search "${query}" -n 3 --json \
    | jq -r '.data.results // [] | .[] | "  - \(.file):\(.start_line) (\(.reason)) score=\(.score)"'
  echo
done

echo "3) Multi-query (batch-style) via get-context"
"${CLI}" "${COMMON[@]}" get-context "error handling" "embedding model" "fuzzy matching" --path . -n 3 --json \
  | jq '{count: length}'
echo

echo "=== DONE ==="
