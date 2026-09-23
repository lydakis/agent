# Sourced by demo.tape before recording starts: a fresh copy of the demo
# project and a throwaway store in a new temporary directory, so the recording
# starts clean. `demo_stop` stops what this created and removes that directory.
#
# By default the model is scripted_model.py and no API key is needed. For a
# recording with a real model, set AGENT_DEMO_LIVE=1, AGENT_MODEL, and that
# provider's key, e.g. AGENT_MODEL=anthropic/claude-sonnet-5 with
# ANTHROPIC_API_KEY; `agent run` then starts the daemon itself.
demo=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$demo/../.." && pwd)
export PATH="$root/.local/target/release:$PATH"
demo_dir=$(mktemp -d "${TMPDIR:-/tmp}/agent-demo.XXXXXX") || return 1
export AGENT_STORE="$demo_dir/state.sqlite" AGENT_SOCKET="$demo_dir/agent.sock"
cp -R "$demo/tally" "$demo_dir/tally"

demo_stop() {
  agent shutdown 2>/dev/null
  [ -n "$demo_model" ] && kill "$demo_model" 2>/dev/null
  rm -rf "$demo_dir"
}

if [ -z "$AGENT_DEMO_LIVE" ]; then
  export AGENT_MODEL=demo/scripted
  python3 "$demo/scripted_model.py" 0 >"$demo_dir/model.port" &
  demo_model=$!
  # Wait until the model has bound its port, and give up if it exited.
  until [ -s "$demo_dir/model.port" ]; do
    kill -0 "$demo_model" 2>/dev/null || { echo "setup: scripted model did not start" >&2; demo_stop; return 1; }
    sleep 0.1
  done
  agent serve --store "$AGENT_STORE" --socket "$AGENT_SOCKET" \
    --provider "demo=responses,http://127.0.0.1:$(cat "$demo_dir/model.port")/v1" >"$demo_dir/serve.log" 2>&1 &
  demo_daemon=$!
  until [ -S "$AGENT_SOCKET" ]; do
    kill -0 "$demo_daemon" 2>/dev/null || { echo "setup: daemon did not start; see $demo_dir/serve.log" >&2; cat "$demo_dir/serve.log" >&2; demo_stop; return 1; }
    sleep 0.1
  done
fi
cd "$demo_dir/tally"
PS1='\[\e[1;34m\]~/tally\[\e[0m\] $ '
