#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if [[ "${CONTEXT_EMBEDDING_MODE:-}" == "stub" ]]; then
  echo "ERROR: CONTEXT_EMBEDDING_MODE=stub; real-embeddings smoke requires a non-stub mode" >&2
  exit 2
fi

export CONTEXT_EMBEDDING_MODE="${CONTEXT_EMBEDDING_MODE:-fast}"

echo "INFO: real embeddings smoke (mode=${CONTEXT_EMBEDDING_MODE})"
echo "INFO: model_dir=${CONTEXT_MODEL_DIR:-./models}"
echo "INFO: allow_cpu=${CONTEXT_ALLOW_CPU:-0}"

# Install models and validate runtime.
cargo run -q -p context-cli --bin context -- --quiet install-models
cargo run -q -p context-cli --bin context -- doctor

# Run only the tests that are ignored in stub-only CI.
cargo test -p context-vector-store -- --ignored
cargo test -p context-indexer -- --ignored
cargo test -p context-search -- --ignored

echo "OK: real embeddings smoke"
