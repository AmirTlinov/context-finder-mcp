#!/bin/bash

CLI="./target/release/context-finder"

echo "=== QUALITY ANALYSIS ==="
echo ""

# 1. Check chunking granularity
echo "1. Chunking Granularity Test"
echo "   embeddings.rs symbols:"
$CLI list-symbols crates/vector-store/src/embeddings.rs 2>&1 | grep -v "INFO" | jq -r '.symbols[] | "   - \(.type) \(.name) (line \(.line))"'
echo ""

echo "   chunker.rs symbols:"
$CLI list-symbols crates/code-chunker/src/chunker.rs 2>&1 | grep -v "INFO" | jq -r '.symbols[] | "   - \(.type) \(.name) (line \(.line))"'
echo ""

# 2. Total chunks vs symbols
echo "2. Coverage Analysis"
total_files=$(find crates -name "*.rs" | wc -l)
echo "   Total RS files: $total_files"
echo "   Total indexed chunks: $(jq '.chunks' .context-finder/index.json 2>/dev/null || echo 'N/A')"
echo ""

# 3. Semantic relevance
echo "3. Semantic Search Relevance Tests"
echo ""

echo "   Query: 'AST parsing tree-sitter'"
$CLI search "AST parsing tree-sitter" -l 3 2>&1 | grep -v "INFO" | jq -r '.results[] | "   [\(.score | tonumber * 100 | floor)]% - \(.file):\(.start_line) \(.symbol // "?")"'
echo ""

echo "   Query: 'embedding vector similarity'"
$CLI search "embedding vector similarity" -l 3 2>&1 | grep -v "INFO" | jq -r '.results[] | "   [\(.score | tonumber * 100 | floor)]% - \(.file):\(.start_line) \(.symbol // "?")"'
echo ""

echo "   Query: 'fuzzy matching'"
$CLI search "fuzzy matching" -l 3 2>&1 | grep -v "INFO" | jq -r '.results[] | "   [\(.score | tonumber * 100 | floor)]% - \(.file):\(.start_line) \(.symbol // "?")"'
echo ""

# 4. Fuzzy accuracy
echo "4. Fuzzy Search Accuracy"
echo ""

echo "   Exact: 'HybridSearch'"
$CLI search "HybridSearch" -l 1 2>&1 | grep -v "INFO" | jq -r '.results[0] | "   [\(.score | tonumber * 100 | floor)]% - \(.file):\(.start_line) \(.symbol // "?")"'
echo ""

echo "   Typo: 'HybridSerch' (missing 'a')"
$CLI search "HybridSerch" -l 1 2>&1 | grep -v "INFO" | jq -r '.results[0] | "   [\(.score | tonumber * 100 | floor)]% - \(.file):\(.start_line) \(.symbol // "?")"'
echo ""

# 5. Check for method extraction
echo "5. Method Extraction Check"
echo "   Searching for 'embed' function:"
$CLI search "embed" -l 3 2>&1 | grep -v "INFO" | jq -r '.results[] | "   \(.file):\(.start_line) - \(.symbol // "no symbol") (type: \(.type // "N/A"))"'
echo ""

echo "=== ANALYSIS COMPLETE ==="
