#!/bin/sh
# Run triage.py against scripted_model.py in a throwaway store: no API key.
# Put a built `agent` on PATH first (or set AGENT_BIN).
set -e
here=$(cd "$(dirname "$0")" && pwd)
dir=$(mktemp -d "${TMPDIR:-/tmp}/triage.XXXXXX")
export AGENT_STORE="$dir/state.sqlite" AGENT_SOCKET="$dir/agent.sock" AGENT_MODEL=scripted/triage
trap '"${AGENT_BIN:-agent}" shutdown >/dev/null 2>&1; kill "$model" 2>/dev/null; rm -rf "$dir"' EXIT
python3 "$here/scripted_model.py" 0 >"$dir/port" &
model=$!
until [ -s "$dir/port" ]; do sleep 0.1; done
export AGENT_PROVIDER="scripted=responses,http://127.0.0.1:$(cat "$dir/port")/v1"
python3 "$here/triage.py" "$here/issues.jsonl"
