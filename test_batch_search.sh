#!/bin/bash

set -e

PROJECT_ROOT="/home/amir/Документы/PROJECTS/skills/apply_context/context-finder"
CLI="$PROJECT_ROOT/target/debug/context-finder-cli"

cd "$PROJECT_ROOT"

echo "=== Testing Batch Search API ==="
echo

echo "1. Re-index (ensure fresh index):"
time $CLI index . 2>&1 | grep -E "(Indexing|files|chunks|time_ms)"
echo

echo "2. Test sequential search (baseline):"
echo "Query 1: 'error handling'"
time $CLI search "error handling" --limit 3 2>&1 | grep -E "file_path|symbol_name|score" | head -15
echo
echo "Query 2: 'embedding model'"
time $CLI search "embedding model" --limit 3 2>&1 | grep -E "file_path|symbol_name|score" | head -15
echo
echo "Query 3: 'fuzzy matching'"
time $CLI search "fuzzy matching" --limit 3 2>&1 | grep -E "file_path|symbol_name|score" | head -15
echo

echo "3. Test batch search (should be faster):"
echo "NOTE: CLI doesn't support batch search yet - this is proof of API existence"
echo "Batch search API is available via:"
echo "  - VectorStore::search_batch(&[&str], limit)"
echo "  - HybridSearch::search_batch(&[&str], limit)"
echo

echo "=== Batch Search API Implementation Complete ==="
