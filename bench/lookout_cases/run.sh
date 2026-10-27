#!/bin/sh
# Build the lookout eval's cases, replay them with each judge, and score
# them. The build is scripted and free; the replay spends the judges' models:
# the lead's fork on the daemon's provider, with this machine's sign-in.
# Usage: bench/lookout_cases/run.sh OUT [JUDGES]
set -eu
OUT=$1
JUDGES=${2:-openai-decisions/gpt-6-luna,lead,openai-decisions/gpt-6-luna+lead}
LEAD_MODEL=${LEAD_MODEL:-chatgpt/gpt-6-sol}
BIN=.local/target/release
python3 -m bench.lookout_cases.build "$OUT" --lead-model "$LEAD_MODEL"
DIR=$(mktemp -d /tmp/lookout-eval.XXXXXX)
"$BIN/agent" serve --store "$OUT/state.sqlite" --provider "${LEAD_MODEL%%/*}" --socket "$DIR/d.sock" \
    >"$OUT/daemon.log" 2>&1 &
DAEMON=$!
trap 'kill $DAEMON; rm -rf "$DIR"' EXIT
while [ ! -S "$DIR/d.sock" ]; do sleep 0.1; done
"$BIN/sonny" lookout --replay --judge "$JUDGES" --store "$OUT/state.sqlite" --socket "$DIR/d.sock" \
    >"$OUT/replay.jsonl"
python3 bench/lookout_score.py "$OUT/replay.jsonl" "$OUT/labels.jsonl" | tee "$OUT/score.txt"
