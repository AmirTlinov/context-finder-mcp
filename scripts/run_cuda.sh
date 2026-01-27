#!/usr/bin/env bash
set -euo pipefail

# Preferred entrypoint (legacy alias: run_cf_cuda.sh).
exec "$(dirname "${BASH_SOURCE[0]}")/run_cf_cuda.sh" "$@"
