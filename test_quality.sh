#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${ROOT_DIR}"

CLI="${CLI:-./target/release/context-finder}"
if [[ ! -x "${CLI}" ]]; then
  echo "[test_quality] CLI not found at ${CLI}. Build it with: cargo build --release -p context-finder-cli" >&2
  exit 1
fi

EMBED_MODE="${CONTEXT_FINDER_EMBEDDING_MODE:-stub}"
COMMON=(--quiet --embed-mode "${EMBED_MODE}")

echo "=== QUALITY ANALYSIS ==="
echo ""

# Ensure an index exists (some checks rely on it).
"${CLI}" "${COMMON[@]}" index . --json >/dev/null

# 1. Check chunking granularity
echo "1. Chunking Granularity Test"
echo "   embeddings.rs symbols:"
"${CLI}" "${COMMON[@]}" list-symbols . --file crates/vector-store/src/embeddings.rs --json \
  | jq -r '.data.symbols[]? | "   - \(.type) \(.name) (line \(.line))"'
echo ""

echo "   chunker.rs symbols:"
"${CLI}" "${COMMON[@]}" list-symbols . --file crates/code-chunker/src/chunker.rs --json \
  | jq -r '.data.symbols[]? | "   - \(.type) \(.name) (line \(.line))"'
echo ""

# 2. Total chunks vs symbols
echo "2. Coverage Analysis"
total_files=$(find crates -name "*.rs" | wc -l | tr -d ' ')
echo "   Total RS files: ${total_files}"

index_path="$(ls .context-finder/indexes/*/index.json 2>/dev/null | head -n 1 || true)"
if [[ -n "${index_path}" ]]; then
  total_chunks="$(jq -r '.id_map | length' "${index_path}" 2>/dev/null || echo 'N/A')"
  echo "   Total indexed chunks: ${total_chunks} (${index_path})"
else
  echo "   Total indexed chunks: N/A (no .context-finder/indexes/*/index.json found)"
fi
echo ""

# 3. Semantic relevance
echo "3. Search Relevance Tests (semantic + fuzzy)"
echo ""

echo "   Query: 'AST parsing tree-sitter'"
"${CLI}" "${COMMON[@]}" search "AST parsing tree-sitter" -n 3 --json \
  | jq -r '.data.results // [] | .[] | "   [\(.score * 100 | floor)]% - \(.file):\(.start_line) \(.symbol // "?")"'
echo ""

echo "   Query: 'embedding vector similarity'"
"${CLI}" "${COMMON[@]}" search "embedding vector similarity" -n 3 --json \
  | jq -r '.data.results // [] | .[] | "   [\(.score * 100 | floor)]% - \(.file):\(.start_line) \(.symbol // "?")"'
echo ""

echo "   Query: 'fuzzy matching'"
"${CLI}" "${COMMON[@]}" search "fuzzy matching" -n 3 --json \
  | jq -r '.data.results // [] | .[] | "   [\(.score * 100 | floor)]% - \(.file):\(.start_line) \(.symbol // "?")"'
echo ""

# 4. Fuzzy accuracy
echo "4. Fuzzy Search Accuracy"
echo ""

echo "   Exact: 'HybridSearch'"
"${CLI}" "${COMMON[@]}" search "HybridSearch" -n 1 --json \
  | jq -r '.data.results // [] | .[0] | "   [\(.score * 100 | floor)]% - \(.file):\(.start_line) \(.symbol // "?")"' \
  2>/dev/null || true
echo ""

echo "   Typo: 'HybridSerch' (missing 'a')"
"${CLI}" "${COMMON[@]}" search "HybridSerch" -n 1 --json \
  | jq -r '.data.results // [] | .[0] | "   [\(.score * 100 | floor)]% - \(.file):\(.start_line) \(.symbol // "?")"' \
  2>/dev/null || true
echo ""

# 5. Check for method extraction
echo "5. Method Extraction Check"
echo "   Searching for 'embed' function:"
"${CLI}" "${COMMON[@]}" search "embed" -n 3 --json \
  | jq -r '.data.results // [] | .[] | "   \(.file):\(.start_line) - \(.symbol // "no symbol") (type: \(.type // "N/A"))"'
echo ""

echo "=== ANALYSIS COMPLETE ==="
