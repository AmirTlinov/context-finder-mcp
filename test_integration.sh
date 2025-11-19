#!/bin/bash

# Integration testing script for context-finder
CLI="./target/release/context-finder"

echo "=== INTEGRATION TESTING CONTEXT-FINDER ==="
echo ""

# Test 1: Semantic search for concepts
echo "TEST 1: Semantic search - 'error handling'"
$CLI search "error handling" -l 5 | jq -r '.results[] | "\(.file):\(.start_line) - \(.symbol // "unknown") (score: \(.score))"'
echo ""

# Test 2: Semantic search for AST parsing
echo "TEST 2: Semantic search - 'AST parsing'"
$CLI search "AST parsing" -l 5 | jq -r '.results[] | "\(.file):\(.start_line) - \(.symbol // "unknown") (score: \(.score))"'
echo ""

# Test 3: Fuzzy search by exact function name
echo "TEST 3: Fuzzy search - 'chunk_by_tokens'"
$CLI search "chunk_by_tokens" -l 5 | jq -r '.results[] | "\(.file):\(.start_line) - \(.symbol // "unknown") (score: \(.score))"'
echo ""

# Test 4: Fuzzy with typo
echo "TEST 4: Fuzzy with typo - 'embeding' (should find 'embedding')"
$CLI search "embeding" -l 5 | jq -r '.results[] | "\(.file):\(.start_line) - \(.symbol // "unknown") (score: \(.score))"'
echo ""

# Test 5: Semantic - vector operations
echo "TEST 5: Semantic - 'vector similarity'"
$CLI search "vector similarity" -l 5 | jq -r '.results[] | "\(.file):\(.start_line) - \(.symbol // "unknown") (score: \(.score))"'
echo ""

# Test 6: Complex query
echo "TEST 6: Semantic - 'chunk code into functions'"
$CLI search "chunk code into functions" -l 5 | jq -r '.results[] | "\(.file):\(.start_line) - \(.symbol // "unknown") (score: \(.score))"'
echo ""

# Test 7: File-specific search
echo "TEST 7: Fuzzy - 'main' (should find main function)"
$CLI search "main" -l 5 | jq -r '.results[] | "\(.file):\(.start_line) - \(.symbol // "unknown") (score: \(.score))"'
echo ""

# Test 8: Check chunking quality
echo "TEST 8: List symbols in chunker.rs"
$CLI list-symbols crates/code-chunker/src/chunker.rs | jq -r '.symbols[] | "\(.line): \(.type) \(.name) (parent: \(.parent // "none"))"'
echo ""

# Test 9: Get context around specific line
echo "TEST 9: Get context around line 50 in chunker.rs"
$CLI get-context crates/code-chunker/src/chunker.rs 50 --window 5 | jq '{symbol, type, parent, line, imports: .imports | length}'
echo ""

echo "=== TESTING COMPLETE ==="
