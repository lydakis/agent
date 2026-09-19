"""Bounded live fleet: N bots submitted at once through one daemon, then waited on.

This is a paid check against a real provider, never part of the test suite.
It reports achieved concurrency from turn timestamps, per-turn latency,
outcomes, tokens, and daemon RSS/thread samples. The provider key comes from
the caller's environment and is never printed or stored.

    .local/venv/bin/python -m bench.live_fleet --bots 32 --model anthropic/claude-sonnet-5 \
        --out .local/bench/fleet-sonnet-32
"""
import argparse
import json
import os
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

import psutil

from .targets import file_hash

PROMPT = 'Run wc -l notes.txt with the shell tool and report just the number.'


def run(agent, out, bots, model, max_connecting, reasoning):
    store = out / 'state.sqlite'
    workspace = out / 'workspace'
    workspace.mkdir(parents=True)
    (workspace / 'notes.txt').write_text('alpha\nbeta\ngamma\n')
    provider = model.split('/', 1)[0]
    env = os.environ.copy()
    common = ['--store', str(store), '--provider', provider, '--model', model, '--reasoning', reasoning,
              '--tools', 'shell,read,write,edit,wait', '--workspace', str(workspace),
              *(['--max-connecting', str(max_connecting)] if max_connecting is not None else [])]
    # One detached turn starts the daemon so every fleet submission races a live one.
    subprocess.run([str(agent), 'run', *common, '--new', '--bot', 'warm', '--detach',
                    'reply with the single word ready'], check=True, capture_output=True, env=env)
    daemon = next(p for p in psutil.process_iter(['cmdline'])
                  if p.info['cmdline'] and 'serve' in p.info['cmdline'] and str(store) in ' '.join(p.info['cmdline']))
    samples, done = [], threading.Event()

    def sample():
        while not done.is_set():
            try:
                samples.append((daemon.memory_info().rss, daemon.num_threads()))
            except psutil.Error:
                break
            time.sleep(.2)

    threading.Thread(target=sample, daemon=True).start()
    started = time.monotonic()
    handles, failures, lock = [], [], threading.Lock()

    def submit(i):
        r = subprocess.run([str(agent), 'run', '--store', str(store), '--workspace', str(workspace), '--new',
                            '--model', model, '--bot', f'f{i}', '--detach', PROMPT],
                           capture_output=True, text=True, env=env)
        with lock:
            if r.returncode == 0:
                handles.append(json.loads(r.stdout)['handle'])
            else:
                failures.append(r.stderr.strip()[:200])

    threads = [threading.Thread(target=submit, args=(i,)) for i in range(bots)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    submit_seconds = time.monotonic() - started
    results = {}

    def waiter(chunk):  # wait accepts at most 64 handles per request
        r = subprocess.run([str(agent), 'wait', '--store', str(store), '--timeout-ms', '600000', *chunk],
                           capture_output=True, text=True, env=env)
        got = json.loads(r.stdout)['results'] if r.stdout.strip() else {
            h: {'error': 'wait_failed', 'detail': r.stderr.strip()[:120]} for h in chunk}
        with lock:
            results.update(got)

    waiters = [threading.Thread(target=waiter, args=(handles[i:i + 64],)) for i in range(0, len(handles), 64)]
    for t in waiters:
        t.start()
    for t in waiters:
        t.join()
    total_seconds = time.monotonic() - started
    done.set()
    turns = []
    for i in range(bots):
        out_text = subprocess.run([str(agent), 'turns', '--store', str(store), '--bot', f'f{i}'],
                                  capture_output=True, text=True, env=env).stdout
        turns.extend(json.loads(out_text) if out_text.strip() else [])
    subprocess.run([str(agent), 'shutdown', '--store', str(store)], capture_output=True, env=env)
    events = sorted([(t['started_ms'], 1) for t in turns] + [(t['finished_ms'] or 0, -1) for t in turns])
    current = peak = 0
    for _, delta in events:
        current += delta
        peak = max(peak, current)
    latency = sorted((t['finished_ms'] or 0) - t['started_ms'] for t in turns)
    statuses = {}
    for t in turns:
        statuses[t['status']] = statuses.get(t['status'], 0) + 1
    errors = {h: v.get('error') or 'pending' for h, v in results.items() if v.get('error') or v.get('pending')}
    rss = [s[0] for s in samples]
    return dict(schema='live_fleet_v1', created_at=datetime.now(timezone.utc).isoformat(), model=model, bots=bots,
                max_connecting=max_connecting, reasoning=reasoning, prompt=PROMPT, binary_sha256=file_hash(agent),
                submitted=len(handles), submit_failures=failures[:5], statuses=statuses, errors=errors,
                peak_overlapping_turns=peak, submit_seconds=round(submit_seconds, 2),
                total_seconds=round(total_seconds, 2),
                turn_ms=dict(p50=latency[len(latency) // 2], p95=latency[max(int(len(latency) * .95) - 1, 0)],
                             max=latency[-1]) if latency else None,
                input_tokens=sum(t['input_tokens'] for t in turns), output_tokens=sum(t['output_tokens'] for t in turns),
                daemon_rss_mib=dict(start=round(rss[0] / 2**20, 2), peak=round(max(rss) / 2**20, 2)) if rss else None,
                daemon_threads_max=max(s[1] for s in samples) if samples else None,
                host=dict(system=os.uname().sysname, machine=os.uname().machine))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    parser.add_argument('--bots', type=int, default=32)
    parser.add_argument('--model', required=True)
    parser.add_argument('--max-connecting', type=int, default=None, help='daemon default (unbounded) when omitted')
    parser.add_argument('--reasoning', default='low')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    out = args.out.resolve()
    if Path.cwd() != root or not out.is_relative_to(root / '.local') or out.exists():
        parser.error('run from the repository root with a new output directory under .local')
    if not (1 <= args.bots <= 1024):
        parser.error('--bots must be between 1 and 1024')
    result = run(args.binary.resolve(), out, args.bots, args.model, args.max_connecting, args.reasoning)
    (out / 'result.json').write_text(json.dumps(result, indent=2))
    print(json.dumps(result))
    return 0 if result['statuses'].get('completed') == args.bots and not result['errors'] else 1


if __name__ == '__main__':
    sys.exit(main())
