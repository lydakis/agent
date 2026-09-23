# Sourced by demo.tape before recording starts: a fresh copy of the demo
# project in /tmp/tally and a throwaway store, so the recording starts clean.
#
# By default the model is scripted_model.py and no API key is needed. For a
# recording with a real model, set AGENT_DEMO_LIVE=1, AGENT_MODEL, and that
# provider's key, e.g. AGENT_MODEL=anthropic/claude-sonnet-5 with
# ANTHROPIC_API_KEY; `agent run` then starts the daemon itself.
demo=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$demo/../.." && pwd)
export PATH="$root/.local/target/release:$PATH"
export AGENT_STORE=/tmp/agent-demo/state.sqlite AGENT_SOCKET=/tmp/agent-demo/agent.sock
demo_stop() {
  agent shutdown 2>/dev/null
  [ -f /tmp/agent-demo/model.pid ] && kill "$(cat /tmp/agent-demo/model.pid)" 2>/dev/null
}
demo_stop
rm -rf /tmp/agent-demo /tmp/tally && mkdir -p /tmp/agent-demo && cp -R "$demo/tally" /tmp/tally
if [ -z "$AGENT_DEMO_LIVE" ]; then
  export AGENT_MODEL=demo/scripted
  python3 "$demo/scripted_model.py" 8765 &
  echo $! >/tmp/agent-demo/model.pid
  agent serve --store "$AGENT_STORE" --socket "$AGENT_SOCKET" \
    --provider demo=responses,http://127.0.0.1:8765/v1 >/tmp/agent-demo/serve.log 2>&1 &
  sleep 1
fi
cd /tmp/tally
PS1='\[\e[1;34m\]~/tally\[\e[0m\] $ '
