#!/usr/bin/env bash
# Exercise the HTTP MCP gateway so the Grok Build remote observer has useful
# native history rows to render: list/read/edit/diff/shell, plus an optional
# approval-gated coding command.
#
# Requires: bash, curl, python3.  It does not use an API key or an LLM.
#
# Start the observer in another terminal first:
#   cargo run -p xai-grok-pager-bin -- mcp server \
#     --workspace /path/to/workspace --transport http --ui
# Then run:
#   ./scripts/mcp-observer-demo.sh --workspace /path/to/workspace
# Or include a shell approval prompt:
#   ./scripts/mcp-observer-demo.sh --workspace /path/to/workspace --approval-demo

set -euo pipefail

endpoint="http://127.0.0.1:8765/mcp"
workspace=""
approval_demo=false

usage() {
  cat <<'EOF'
Usage: scripts/mcp-observer-demo.sh --workspace PATH [options]

Options:
  --endpoint URL       Gateway endpoint (default: http://127.0.0.1:8765/mcp)
  --approval-demo      End with `cargo check`, which waits for Allow once/Deny
  -h, --help           Show this help
EOF
}

while (($#)); do
  case "$1" in
    --workspace) workspace=${2:?missing workspace path}; shift 2 ;;
    --endpoint) endpoint=${2:?missing endpoint URL}; shift 2 ;;
    --approval-demo) approval_demo=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ -z "$workspace" ]]; then
  echo "--workspace is required" >&2
  usage >&2
  exit 2
fi
if [[ ! -d "$workspace" ]]; then
  echo "Workspace does not exist: $workspace" >&2
  exit 2
fi

for command in curl python3; do
  command -v "$command" >/dev/null || {
    echo "Missing required command: $command" >&2
    exit 1
  }
done

temp_dir=$(mktemp -d)
trap 'rm -rf "$temp_dir"' EXIT
headers="$temp_dir/headers"
body="$temp_dir/body"
session_id=""
request_id=0

pretty_body() {
  # rmcp's Streamable HTTP transport chooses an SSE response when requested.
  # Extract each JSON-RPC `data:` frame for readable terminal output while
  # retaining a plain JSON fallback for servers that do not use SSE.
  local sse_json
  sse_json=$(sed -n 's/^data: //p' "$body" | sed '/^$/d')
  if [[ -n "$sse_json" ]]; then
    printf '%s\n' "$sse_json" | python3 -m json.tool 2>/dev/null || printf '%s\n' "$sse_json"
  else
    python3 -m json.tool "$body" 2>/dev/null || cat "$body"
  fi
  printf '\n'
}

post() {
  local payload=$1
  local -a args=(
    --silent --show-error --request POST "$endpoint"
    --header 'Content-Type: application/json'
    --header 'Accept: application/json, text/event-stream'
    --data "$payload"
    --output "$body"
  )
  if [[ -n "$session_id" ]]; then
    args+=(--header "Mcp-Session-Id: $session_id")
  fi
  curl "${args[@]}"
}

json_request() {
  local method=$1
  local params=${2:-null}
  local id=${3:-}
  python3 - "$method" "$params" "$id" <<'PY'
import json
import sys

method, params, request_id = sys.argv[1:]
message = {"jsonrpc": "2.0", "method": method}
if params != "null":
    message["params"] = json.loads(params)
if request_id:
    message["id"] = int(request_id)
print(json.dumps(message))
PY
}

call() {
  local name=$1
  local args=$2
  request_id=$((request_id + 1))
  echo
  echo "==> tools/call $name"
  post "$(json_request tools/call "{\"name\": \"$name\", \"arguments\": $args}" "$request_id")"
  pretty_body
}

echo "==> initialize $endpoint"
init_payload=$(json_request initialize \
  '{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"mcp-observer-demo","version":"1.0"}}' \
  1)
curl --silent --show-error --request POST "$endpoint" \
  --header 'Content-Type: application/json' \
  --header 'Accept: application/json, text/event-stream' \
  --data "$init_payload" \
  --dump-header "$headers" \
  --output "$body"
pretty_body

session_id=$(awk 'tolower($1) == "mcp-session-id:" { print $2 }' "$headers" | tr -d '\r' | tail -n 1)
if [[ -z "$session_id" ]]; then
  echo "Gateway did not return Mcp-Session-Id; initialize failed." >&2
  exit 1
fi

# Complete the MCP lifecycle before issuing calls.
post "$(json_request notifications/initialized)"

request_id=1
echo
echo '==> tools/list'
request_id=$((request_id + 1))
post "$(json_request tools/list '{}' "$request_id")"
pretty_body

# The fixture remains in the workspace so its diffs and read output remain
# meaningful while navigating the observer history.
fixture="mcp-observer-fixture.txt"
call list_dir '{"target_directory":"."}'
call search_replace "{\"file_path\":\"$fixture\",\"old_string\":\"\",\"new_string\":\"first line\\nsecond line\\n\",\"replace_all\":false}"
call read_file "{\"target_file\":\"$fixture\"}"
call search_replace "{\"file_path\":\"$fixture\",\"old_string\":\"second line\",\"new_string\":\"second line (edited through MCP)\",\"replace_all\":false}"
call read_file "{\"target_file\":\"$fixture\"}"
call run_terminal_cmd '{"command":"pwd","description":"Show the workspace used by this MCP observer demo."}'

if "$approval_demo"; then
  echo
  echo 'The next call waits for the observer approval UI. Use Up/Down + Enter there.'
  call run_terminal_cmd '{"command":"cargo check","description":"Verify the workspace after the observer demo edits."}'
fi

echo "Demo complete. Fixture retained at $workspace/$fixture"
