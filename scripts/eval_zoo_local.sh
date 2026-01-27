#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

root="/home/amir/Документы/projects"
out_dir=""
limit="80"
max_depth="6"
max_chars="2000"
include_worktrees="true"
strict="true"
compare_to=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --root)
      root="$2"
      shift 2
      ;;
    --out-dir)
      out_dir="$2"
      shift 2
      ;;
    --limit)
      limit="$2"
      shift 2
      ;;
    --max-depth)
      max_depth="$2"
      shift 2
      ;;
    --max-chars)
      max_chars="$2"
      shift 2
      ;;
    --include-worktrees)
      include_worktrees="true"
      shift 1
      ;;
    --no-include-worktrees)
      include_worktrees="false"
      shift 1
      ;;
    --strict)
      strict="true"
      shift 1
      ;;
    --no-strict)
      strict="false"
      shift 1
      ;;
    --compare-to)
      compare_to="$2"
      shift 2
      ;;
    --help|-h)
      cat <<'EOF'
eval_zoo_local.sh

Runs the real-repo MCP zoo runner and writes JSON+MD artifacts.

Flags:
  --root <dir>                 Root directory to scan for git repos
  --out-dir <dir>              Output directory (default: /tmp/context_zoo_runs/<timestamp>)
  --limit <N>                  Max repos (default: 80)
  --max-depth <N>              Scan depth (default: 6)
  --max-chars <N>              Tool max_chars budget (default: 2000)
  --include-worktrees          Scan repos under .worktrees/ (default)
  --no-include-worktrees
  --strict                     Fail-closed per-call thresholds (default)
  --no-strict
  --compare-to <report.json>   Compare summary metrics vs previous report
EOF
      exit 0
      ;;
    *)
      echo "ERROR: unknown arg: $1" >&2
      exit 2
      ;;
  esac
done

if [[ -z "${out_dir}" ]]; then
  ts="$(date -u +%Y%m%dT%H%M%SZ)"
  out_dir="/tmp/context_zoo_runs/${ts}"
fi

mkdir -p "${out_dir}"
out_json="${out_dir}/zoo_report.json"
out_md="${out_dir}/zoo_report.md"

args=(--root "${root}" --max-depth "${max_depth}" --limit "${limit}" --max-chars "${max_chars}")
if [[ "${include_worktrees}" == "true" ]]; then
  args+=(--include-worktrees)
fi
if [[ "${strict}" == "true" ]]; then
  args+=(--strict)
fi
args+=(--out-json "${out_json}" --out-md "${out_md}")

# Deterministic local eval mode: no model downloads.
export CONTEXT_EMBEDDING_MODE=stub

cargo run -q -p context-mcp --bin context-mcp-eval-zoo -- "${args[@]}"

if [[ -n "${compare_to}" ]]; then
  python3 scripts/eval_zoo_compare.py --current "${out_json}" --previous "${compare_to}"
fi

echo "OK: wrote ${out_json} and ${out_md}"
