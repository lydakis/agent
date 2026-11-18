"""Time `memory said`, the check-in gate's read of what the person wrote,
over a store of many agents, on two builds against the same store.

Synthetic provider, no spend. Every agent gets one turn from a person, so
each has history to look through; then a handful get the person's words
inside the gate's window: plain words, an answer to an ask, a reaction.
The base build seeds the store, and each build in turn runs its own daemon
on it and times its own Sonny's `memory said SINCE` (the gate's twenty
minutes, as the automation skill writes it) and over the last day (as the
nightly cleanup reads it), each run several times.

    python3 -m bench.said_scale --base DIR --new DIR --agents 1000 10000 --out FILE

DIR holds a build's `agent` and `sonny`. Prints and writes a JSON summary.
"""
import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.runtime_client import Client  # noqa: E402
from bench.synthetic_model import start  # noqa: E402

RUNS = 5
GATE_MS = 20 * 60 * 1000
DAY_MS = 24 * 60 * 60 * 1000


def seed(binary, store, socket, url, agents):
    client = Client(binary, store, url, tools='echo',
                    extra=('--socket', str(socket), '--max-active', '64'))
    try:
        workspace = str(store.parent)
        for n in range(agents):
            assert 'result' in client.request('create', bot=f'p{n % 50}.a{n}', workspace=workspace)
        # One turn each, from a person, 64 at a time.
        pending = []
        for n in range(agents):
            answer = client.request('submit', bot=f'p{n % 50}.a{n}', request_id='r1', prompt='work',
                                    delivery='queue')
            pending.append((f'p{n % 50}.a{n}', answer['result']['message']))
            if len(pending) >= 64:
                for bot, message in pending:
                    client.finished(message, timeout=60)
                pending.clear()
        for bot, message in pending:
            client.finished(message, timeout=60)
        # The person's words in the gate's window on a few agents.
        words = ['Keep Bedrock out of evals.', 'You asked (ask:1/c1): Ship it now?\n\nLater', '👍']
        for n in range(10):
            bot = f'p{n % 50}.a{n}'
            answer = client.request('submit', bot=bot, request_id='r2', prompt=words[n % 3],
                                    delivery='queue')
            client.finished(answer['result']['message'], timeout=60)
    finally:
        client.close()


def timed(build, store, socket, url, since_ms):
    daemon = subprocess.Popen([str(build / 'agent'), 'serve', '--store', str(store), '--socket', str(socket),
                               '--provider', f'openai=responses,{url}'],
                              stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 30
        while not socket.exists() and time.monotonic() < deadline:
            time.sleep(.05)
        env = {**os.environ, 'AGENT_SOCKET': str(socket), 'AGENT_STORE': str(store),
               'HOME': str(store.parent)}
        times, said = [], None
        for _ in range(RUNS):
            before = time.monotonic()
            done = subprocess.run([str(build / 'sonny'), 'memory', 'said', str(since_ms)], env=env,
                                  capture_output=True, text=True, timeout=600)
            times.append((time.monotonic() - before) * 1000)
            assert done.returncode == 0, done.stderr
            said = json.loads(done.stdout)
        return {'median_ms': round(statistics.median(times), 1), 'max_ms': round(max(times), 1),
                'said': len(said['said'])}
    finally:
        daemon.stdin.close()
        daemon.terminate()
        daemon.wait(timeout=30)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--base', type=Path, required=True)
    parser.add_argument('--new', type=Path, required=True)
    parser.add_argument('--agents', type=int, nargs='+', default=[1000, 10000])
    parser.add_argument('--out', type=Path)
    args = parser.parse_args()
    server, url = start()
    results = []
    for agents in args.agents:
        folder = Path(tempfile.mkdtemp(prefix='said-scale-'))
        try:
            store, socket = folder / 'state.sqlite', folder / 'agent.sock'
            started = time.monotonic()
            seed(args.base / 'agent', store, socket, url, agents)
            row = {'agents': agents, 'seed_s': round(time.monotonic() - started, 1)}
            now = int(time.time() * 1000)
            for name, build in (('main', args.base), ('branch', args.new)):
                row[name] = {'gate': timed(build, store, socket, url, now - GATE_MS),
                             'day': timed(build, store, socket, url, now - DAY_MS)}
            results.append(row)
            print(json.dumps(row), flush=True)
        finally:
            shutil.rmtree(folder, ignore_errors=True)
    server.shutdown()
    if args.out:
        args.out.write_text(json.dumps(results, indent=2) + '\n')


if __name__ == '__main__':
    main()
