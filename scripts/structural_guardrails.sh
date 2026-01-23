#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

cfg="scripts/structural_guardrails.txt"
if [[ ! -f "${cfg}" ]]; then
  echo "ERROR: missing ${cfg}" >&2
  exit 2
fi

failed=0

while IFS= read -r line; do
  line="${line%%$'\r'}"
  [[ -z "${line}" ]] && continue
  [[ "${line}" =~ ^# ]] && continue

  path="$(awk '{print $1}' <<<"${line}")"
  max_lines="$(awk '{print $2}' <<<"${line}")"

  if [[ -z "${path}" || -z "${max_lines}" ]]; then
    echo "ERROR: invalid guardrail line: ${line}" >&2
    exit 2
  fi

  if [[ ! -f "${path}" ]]; then
    echo "WARN: guardrail path missing (skipping): ${path}" >&2
    continue
  fi

  actual_lines=$(wc -l <"${path}" | tr -d ' ')
  if [[ "${actual_lines}" -gt "${max_lines}" ]]; then
    echo "FAIL: ${path} lines=${actual_lines} > max_lines=${max_lines}" >&2
    failed=1
  else
    echo "OK: ${path} lines=${actual_lines} <= max_lines=${max_lines}"
  fi
done <"${cfg}"

exit "${failed}"
