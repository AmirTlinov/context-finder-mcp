#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPS_DIR="${ROOT_DIR}/.deps/ort_cuda"
BIN="${ROOT_DIR}/target/release/context-finder"

if [[ ! -x "${BIN}" ]]; then
  echo "[run_cf_cuda] binary not found at ${BIN}, build first (cargo build --release)" >&2
  exit 1
fi

if [[ ! -d "${DEPS_DIR}" ]]; then
  echo "[run_cf_cuda] CUDA deps not found. Run scripts/setup_cuda_deps.sh first." >&2
  exit 1
fi

export LD_LIBRARY_PATH="${DEPS_DIR}:${LD_LIBRARY_PATH:-}"
export ORT_LIB_LOCATION="${DEPS_DIR}"

exec "${BIN}" "$@"
