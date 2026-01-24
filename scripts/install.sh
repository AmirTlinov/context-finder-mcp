#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

INSTALL_CLI=1
for arg in "${@:-}"; do
  case "$arg" in
    --mcp-only) INSTALL_CLI=0 ;;
    *)
      echo "Unknown arg: $arg" >&2
      echo "Usage: scripts/install.sh [--mcp-only]" >&2
      exit 2
      ;;
  esac
done

echo "Installing MCP server binaries..." >&2
cargo install --path crates/mcp-server --locked

if [[ "$INSTALL_CLI" == "1" ]]; then
  echo "Installing CLI binaries (optional, but recommended for install-models/doctor)..." >&2
  cargo install --path crates/cli --locked
fi

echo "Done." >&2
