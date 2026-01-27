#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

scripts/validate_contracts.sh
bash scripts/structural_guardrails.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
CONTEXT_EMBEDDING_MODE=stub cargo test --workspace

# Contract conformance smoke (HTTP surface).
bash scripts/validate_http_conformance.sh

tmp_json="$(mktemp)"
tmp_repo="$(mktemp -d)"
cleanup() {
  rm -f "${tmp_json}"
  rm -rf "${tmp_repo}"
}
trap cleanup EXIT

# Make the eval step hermetic: do not depend on existing per-repo state under
# `.agents/mcp/.context` or legacy `.context*` dirs from prior local runs.
tar \
  --exclude='./target' \
  --exclude='./.git' \
  --exclude='./.agents' \
  --exclude='./.branchmind_rust' \
  --exclude='./.context' \
  --exclude='./.context-finder' \
  --exclude='./.fastembed_cache' \
  --exclude='./.deps' \
  -cf - . | tar -C "${tmp_repo}" -xf -

CONTEXT_EMBEDDING_MODE=stub cargo run -q -p context-cli --bin context -- index "${tmp_repo}" \
  --force --json --quiet >/dev/null

CONTEXT_EMBEDDING_MODE=stub cargo run -q -p context-cli --bin context -- eval "${tmp_repo}" \
  --dataset "${tmp_repo}/datasets/golden_stub_smoke.json" \
  --json --quiet > "${tmp_json}"

TMP_JSON="${tmp_json}" python3 - <<'PY'
import json, sys, os

path = os.environ.get("TMP_JSON") or ""
if not path:
    # shell passes via heredoc env below; fallback to fixed path if needed
    path = sys.argv[1] if len(sys.argv) > 1 else ""
if not path:
    print("ERROR: missing eval json path", file=sys.stderr)
    sys.exit(2)

with open(path) as f:
    data = json.load(f)

run = data["data"]["runs"][0]
summary = run["summary"]

mean_recall = float(summary["mean_recall"])
mean_mrr = float(summary["mean_mrr"])

min_recall = 0.95
min_mrr = 0.80

if mean_recall < min_recall or mean_mrr < min_mrr:
    print("ERROR: eval regression on datasets/golden_stub_smoke.json", file=sys.stderr)
    print(f"mean_recall={mean_recall:.4f} (min {min_recall})", file=sys.stderr)
    print(f"mean_mrr={mean_mrr:.4f} (min {min_mrr})", file=sys.stderr)
    sys.exit(1)

print(f"OK: eval stub smoke mean_recall={mean_recall:.4f} mean_mrr={mean_mrr:.4f}")
PY
