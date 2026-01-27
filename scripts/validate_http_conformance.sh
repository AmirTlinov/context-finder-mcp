#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

require_bin() {
  local name="$1"
  if ! command -v "${name}" >/dev/null 2>&1; then
    echo "ERROR: missing required binary: ${name}" >&2
    exit 2
  fi
}

require_bin curl
require_bin python3

tmp_dir="$(mktemp -d)"
tmp_body="$(mktemp)"
tmp_headers="$(mktemp)"
server_pid=""

cleanup() {
  if [[ -n "${server_pid}" ]]; then
    kill "${server_pid}" >/dev/null 2>&1 || true
    wait "${server_pid}" >/dev/null 2>&1 || true
  fi
  rm -rf "${tmp_dir}" "${tmp_body}" "${tmp_headers}"
}
trap cleanup EXIT

pick_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

wait_http() {
  local url="$1"
  for _ in $(seq 1 80); do
    if curl -sS "${url}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.05
  done
  echo "ERROR: server did not become ready: ${url}" >&2
  return 1
}

validate_json() {
  local kind="$1"
  local json_path="$2"
  python3 - "${kind}" "${json_path}" <<'PY'
import json
import sys
from pathlib import Path

kind = sys.argv[1]
json_path = Path(sys.argv[2])

root = Path.cwd()

def load(rel: str) -> dict:
    return json.loads((root / rel).read_text(encoding="utf-8"))

def fail(msg: str) -> None:
    raise SystemExit(msg)

def is_int(v) -> bool:
    return isinstance(v, int) and not isinstance(v, bool)

def check_required(obj: dict, required: list[str], where: str) -> None:
    missing = [k for k in required if k not in obj]
    if missing:
        fail(f"{where}: missing required keys: {missing}")

def check_allowed(obj: dict, allowed: set[str], where: str) -> None:
    extra = sorted(set(obj.keys()) - allowed)
    if extra:
        fail(f"{where}: unexpected keys: {extra}")

def check_type(schema_type, value, where: str) -> None:
    if schema_type is None:
        return
    if isinstance(schema_type, list):
        for t in schema_type:
            try:
                check_type(t, value, where)
                return
            except SystemExit:
                pass
        fail(f"{where}: expected types {schema_type}, got {type(value).__name__}")

    t = schema_type
    if t == "null":
        if value is not None:
            fail(f"{where}: expected null")
        return
    if t == "string":
        if not isinstance(value, str):
            fail(f"{where}: expected string")
        return
    if t == "boolean":
        if not isinstance(value, bool):
            fail(f"{where}: expected boolean")
        return
    if t == "integer":
        if not is_int(value):
            fail(f"{where}: expected integer")
        return
    if t == "number":
        if not (is_int(value) or isinstance(value, float)):
            fail(f"{where}: expected number")
        return
    if t == "object":
        if not isinstance(value, dict):
            fail(f"{where}: expected object")
        return
    if t == "array":
        if not isinstance(value, list):
            fail(f"{where}: expected array")
        return

def check_enum(enum: list, value, where: str) -> None:
    if value not in enum:
        fail(f"{where}: expected one of {enum}, got {value!r}")

def check_minimum(minimum: int | float, value, where: str) -> None:
    if value is None:
        return
    if is_int(value) or isinstance(value, float):
        if value < minimum:
            fail(f"{where}: expected >= {minimum}, got {value}")

def validate_next_action_schema(action: dict, schema: dict, where: str) -> None:
    check_type("object", action, where)
    required = schema.get("required", [])
    props = schema.get("properties", {})
    allowed = set(props.keys())
    check_required(action, required, where)
    check_allowed(action, allowed, where)
    check_type("string", action.get("tool"), f"{where}.tool")
    check_type("object", action.get("args"), f"{where}.args")
    check_type("string", action.get("reason"), f"{where}.reason")

def validate_error_envelope(err: dict, schema: dict, next_action_schema: dict, where: str) -> None:
    check_type("object", err, where)
    required = schema.get("required", [])
    props = schema.get("properties", {})
    allowed = set(props.keys())
    check_required(err, required, where)
    check_allowed(err, allowed, where)

    check_type("string", err.get("code"), f"{where}.code")
    check_type("string", err.get("message"), f"{where}.message")

    # details: any JSON value
    hint = err.get("hint")
    if hint is not None:
        check_type("string", hint, f"{where}.hint")

    next_actions = err.get("next_actions")
    check_type("array", next_actions, f"{where}.next_actions")
    for i, item in enumerate(next_actions or []):
        validate_next_action_schema(item, next_action_schema, f"{where}.next_actions[{i}]")

def validate_health_report(obj: dict, schema: dict) -> None:
    where = "health_report"
    check_type("object", obj, where)
    required = schema.get("required", [])
    props = schema.get("properties", {})
    allowed = set(props.keys())
    check_required(obj, required, where)
    check_allowed(obj, allowed, where)

    status_schema = props.get("status", {})
    check_type(status_schema.get("type"), obj.get("status"), f"{where}.status")
    if "enum" in status_schema:
        check_enum(status_schema["enum"], obj.get("status"), f"{where}.status")

    failures_schema = props.get("failures", {})
    check_type("array", obj.get("failures"), f"{where}.failures")
    for i, item in enumerate(obj.get("failures") or []):
        check_type("string", item, f"{where}.failures[{i}]")

    for key, prop_schema in props.items():
        if key in ("status", "failures"):
            continue
        if key not in obj:
            continue
        value = obj[key]
        check_type(prop_schema.get("type"), value, f"{where}.{key}")
        if "enum" in prop_schema:
            check_enum(prop_schema["enum"], value, f"{where}.{key}")
        if "minimum" in prop_schema:
            check_minimum(prop_schema["minimum"], value, f"{where}.{key}")

def validate_index_state(index_state: dict, schema: dict, where: str) -> None:
    check_type("object", index_state, where)
    required = schema.get("required", [])
    props = schema.get("properties", {})
    allowed = set(props.keys())
    check_required(index_state, required, where)
    check_allowed(index_state, allowed, where)

    schema_version = index_state.get("schema_version")
    check_type("integer", schema_version, f"{where}.schema_version")
    if schema_version != 1:
        fail(f"{where}.schema_version: expected 1, got {schema_version}")

    check_type("string", index_state.get("model_id"), f"{where}.model_id")
    check_type("string", index_state.get("profile"), f"{where}.profile")
    check_type("boolean", index_state.get("stale"), f"{where}.stale")

    # Minimal structural checks for nested objects.
    check_type("object", index_state.get("project_watermark"), f"{where}.project_watermark")
    watermark = index_state.get("project_watermark") or {}
    if "kind" in watermark:
        check_type("string", watermark.get("kind"), f"{where}.project_watermark.kind")
        if watermark.get("kind") not in ("git", "filesystem"):
            fail(f"{where}.project_watermark.kind: invalid kind")

    check_type("object", index_state.get("index"), f"{where}.index")
    index_obj = index_state.get("index") or {}
    if "exists" in index_obj:
        check_type("boolean", index_obj.get("exists"), f"{where}.index.exists")

def validate_command_response(
    obj: dict,
    schema: dict,
    error_schema: dict,
    next_action_schema: dict,
    index_state_schema: dict,
) -> None:
    where = "command_response"
    check_type("object", obj, where)
    required = schema.get("required", [])
    props = schema.get("properties", {})
    allowed = set(props.keys())
    check_required(obj, required, where)
    check_allowed(obj, allowed, where)

    status_schema = props.get("status", {})
    check_type(status_schema.get("type"), obj.get("status"), f"{where}.status")
    check_enum(status_schema.get("enum", []), obj.get("status"), f"{where}.status")

    if "message" in obj:
        check_type("string", obj.get("message"), f"{where}.message")

    if "hints" in obj:
        hints = obj.get("hints")
        check_type("array", hints, f"{where}.hints")
        hint_item_schema = props.get("hints", {}).get("items", {}).get("properties", {})
        hint_required = props.get("hints", {}).get("items", {}).get("required", [])
        hint_allowed = set(hint_item_schema.keys())
        hint_type_enum = hint_item_schema.get("type", {}).get("enum", [])
        for i, item in enumerate(hints or []):
            item_where = f"{where}.hints[{i}]"
            check_type("object", item, item_where)
            check_required(item, hint_required, item_where)
            check_allowed(item, hint_allowed, item_where)
            check_type("string", item.get("type"), f"{item_where}.type")
            check_enum(hint_type_enum, item.get("type"), f"{item_where}.type")
            check_type("string", item.get("text"), f"{item_where}.text")

    if "next_actions" in obj:
        next_actions = obj.get("next_actions")
        check_type("array", next_actions, f"{where}.next_actions")
        for i, item in enumerate(next_actions or []):
            validate_next_action_schema(item, next_action_schema, f"{where}.next_actions[{i}]")

    meta = obj.get("meta")
    check_type("object", meta, f"{where}.meta")
    meta_schema = props.get("meta", {})
    meta_required = meta_schema.get("required", [])
    meta_props = meta_schema.get("properties", {})
    meta_allowed = set(meta_props.keys())
    check_required(meta, meta_required, f"{where}.meta")
    check_allowed(meta, meta_allowed, f"{where}.meta")

    # index_state can be null or object.
    if "index_state" not in meta:
        fail(f"{where}.meta: missing index_state")
    index_state = meta.get("index_state")
    if index_state is not None:
        validate_index_state(index_state, index_state_schema, f"{where}.meta.index_state")

    status = obj.get("status")
    if status == "error":
        if "error" not in obj or obj.get("error") is None:
            fail(f"{where}: status=error but error envelope is missing")

    if "error" in obj and obj.get("error") is not None:
        validate_error_envelope(
            obj.get("error"),
            error_schema,
            next_action_schema,
            f"{where}.error",
        )

data = json.loads(json_path.read_text(encoding="utf-8"))

if kind == "health":
    validate_health_report(data, load("contracts/command/v1/health_report.schema.json"))
elif kind == "command_response":
    validate_command_response(
        data,
        load("contracts/command/v1/command_response.schema.json"),
        load("contracts/command/v1/error.schema.json"),
        load("contracts/command/v1/next_action.schema.json"),
        load("contracts/command/v1/index_state.schema.json"),
    )
else:
    fail(f"unknown kind: {kind}")

print("OK")
PY
}

mkdir -p "${tmp_dir}/repo"
cat >"${tmp_dir}/repo/README.md" <<'EOF'
# http conformance smoke
EOF

port="$(pick_port)"

# No-auth mode: loopback bind, no token.
CONTEXT_EMBEDDING_MODE=stub \
  cargo run -q -p context-cli --bin context -- serve-http \
  --bind "127.0.0.1:${port}" \
  --cache-backend memory \
  >/dev/null 2>&1 &
server_pid="$!"

wait_http "http://127.0.0.1:${port}/health"

curl -fsS "http://127.0.0.1:${port}/health" >"${tmp_body}"
validate_json health "${tmp_body}" >/dev/null

curl -fsS -X POST "http://127.0.0.1:${port}/command" \
  -H "Content-Type: application/json" \
  -d "$(python3 - <<PY
import json
print(json.dumps({
  'action': 'map',
  'payload': {
    'project': '${tmp_dir}/repo',
    'max_depth': 2,
  }
}))
PY
)" >"${tmp_body}"
validate_json command_response "${tmp_body}" >/dev/null

kill "${server_pid}" >/dev/null 2>&1 || true
wait "${server_pid}" >/dev/null 2>&1 || true
server_pid=""

# Auth mode: token set => auth required.
port="$(pick_port)"
export CONTEXT_AUTH_TOKEN="test-token"

CONTEXT_EMBEDDING_MODE=stub \
  cargo run -q -p context-cli --bin context -- serve-http \
  --bind "127.0.0.1:${port}" \
  --cache-backend memory \
  >/dev/null 2>&1 &
server_pid="$!"

wait_http "http://127.0.0.1:${port}/health"

status="$(curl -sS -o "${tmp_body}" -D "${tmp_headers}" -w '%{http_code}' "http://127.0.0.1:${port}/health")"
if [[ "${status}" != "401" ]]; then
  echo "ERROR: expected 401 for /health without auth, got ${status}" >&2
  exit 1
fi
if ! grep -i -q '^WWW-Authenticate: Bearer' "${tmp_headers}"; then
  echo "ERROR: missing WWW-Authenticate: Bearer header" >&2
  exit 1
fi
validate_json command_response "${tmp_body}" >/dev/null
validate_json command_response "${tmp_body}" >/dev/null

status="$(curl -sS -o "${tmp_body}" -w '%{http_code}' -X POST "http://127.0.0.1:${port}/command" \
  -H "Content-Type: application/json" \
  -d '{"action":"map","payload":{}}')"
if [[ "${status}" != "401" ]]; then
  echo "ERROR: expected 401 for /command without auth, got ${status}" >&2
  exit 1
fi
validate_json command_response "${tmp_body}" >/dev/null
validate_json command_response "${tmp_body}" >/dev/null

status="$(curl -sS -o "${tmp_body}" -w '%{http_code}' -X POST "http://127.0.0.1:${port}/command" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer ${CONTEXT_AUTH_TOKEN}" \
  -d "$(python3 - <<PY
import json
print(json.dumps({
  'action': 'map',
  'payload': {
    'project': '${tmp_dir}/repo',
    'max_depth': 2,
  }
}))
PY
)")"
if [[ "${status}" != "200" ]]; then
  echo "ERROR: expected 200 for /command with auth, got ${status}" >&2
  exit 1
fi
validate_json command_response "${tmp_body}" >/dev/null
validate_json command_response "${tmp_body}" >/dev/null

echo "OK: http contract conformance"
