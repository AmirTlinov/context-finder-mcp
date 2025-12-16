#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPS_DIR="${ROOT_DIR}/.deps/ort_cuda"
BIN="${ROOT_DIR}/target/release/context-finder"

if [[ ! -x "${BIN}" ]]; then
  echo "[run_cf_cuda] binary not found at ${BIN}, build first (cargo build --release)" >&2
  exit 1
fi

if [[ "${ORT_DISABLE_CUDA:-0}" != "1" ]]; then
  if [[ ! -d "${DEPS_DIR}" ]]; then
    echo "[run_cf_cuda] CUDA deps not found. Run scripts/setup_cuda_deps.sh first." >&2
    exit 1
  fi
  export LD_LIBRARY_PATH="${DEPS_DIR}:${LD_LIBRARY_PATH:-}"
  export ORT_LIB_LOCATION="${DEPS_DIR}"
  export ORT_DISABLE_TENSORRT=1
  export ORT_STRATEGY=system
  export ORT_USE_CUDA=1
  export ORT_DYLIB_PATH="${DEPS_DIR}"
else
  echo "[run_cf_cuda] ORT_DISABLE_CUDA=1 → running without local CUDA libraries" >&2
fi

exec "${BIN}" "$@"
