#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

export ROOT_DIR

python3 - <<'PY'
import json
import os
import re
from pathlib import Path

root = Path(os.environ["ROOT_DIR"]).resolve()
contracts = root / "contracts"

def fail(msg: str) -> None:
    raise SystemExit(f"contracts validation failed: {msg}")

def to_snake_case(name: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()

# 1) Parse all JSON under contracts/
json_files = sorted(contracts.rglob("*.json"))
if not json_files:
    fail("no *.json files found under contracts/")

for p in json_files:
    try:
        json.loads(p.read_text(encoding="utf-8"))
    except Exception as e:
        fail(f"invalid JSON: {p.relative_to(root)} ({e})")

# 2) OpenAPI sanity + $ref existence
openapi_path = contracts / "http" / "v1" / "openapi.json"
openapi = json.loads(openapi_path.read_text(encoding="utf-8"))
for key in ("openapi", "info", "paths"):
    if key not in openapi:
        fail(f"openapi missing top-level key: {key}")

schemas = openapi.get("components", {}).get("schemas", {})
base_dir = openapi_path.parent
for name, schema in schemas.items():
    ref = schema.get("$ref")
    if not ref:
        continue
    if ref.startswith("#"):
        continue
    ref_path = (base_dir / ref).resolve()
    if not ref_path.exists():
        fail(f"openapi schema $ref not found: components.schemas.{name} -> {ref}")

# 3) Keep action enums aligned with Rust source of truth
request_schema_path = contracts / "command" / "v1" / "command_request.schema.json"
request_schema = json.loads(request_schema_path.read_text(encoding="utf-8"))
actual_actions = request_schema["properties"]["action"]["enum"]

domain_rs = root / "crates" / "cli" / "src" / "command" / "domain.rs"
domain_text = domain_rs.read_text(encoding="utf-8")

def parse_rust_enum_variants(text: str, enum_name: str) -> list[str]:
    m = re.search(rf"pub enum {re.escape(enum_name)}\s*\{{(.*?)\n\}}", text, re.S)
    if not m:
        fail(f"cannot find Rust enum: {enum_name} in {domain_rs.relative_to(root)}")
    body = m.group(1)
    variants = []
    for line in body.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        line = line.split("//", 1)[0].strip()
        if not line:
            continue
        line = line.rstrip(",")
        if re.fullmatch(r"[A-Za-z][A-Za-z0-9_]*", line):
            variants.append(line)
    if not variants:
        fail(f"no variants parsed for enum {enum_name}")
    return variants

expected_actions = [to_snake_case(v) for v in parse_rust_enum_variants(domain_text, "CommandAction")]
if actual_actions != expected_actions:
    fail(
        "action enum mismatch:\n"
        f"  schema: {actual_actions}\n"
        f"  rust:   {expected_actions}"
    )

# 4) Keep hint kinds aligned with Rust source of truth
response_schema_path = contracts / "command" / "v1" / "command_response.schema.json"
response_schema = json.loads(response_schema_path.read_text(encoding="utf-8"))
actual_hint_types = response_schema["properties"]["hints"]["items"]["properties"]["type"]["enum"]
expected_hint_types = [
    to_snake_case(v) for v in parse_rust_enum_variants(domain_text, "HintKind")
]
if actual_hint_types != expected_hint_types:
    fail(
        "hint type enum mismatch:\n"
        f"  schema: {actual_hint_types}\n"
        f"  rust:   {expected_hint_types}"
    )

print(f"OK: contracts validated ({len(json_files)} JSON files)")
PY
