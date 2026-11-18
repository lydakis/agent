"""How long a window's reads and a new connection wait while the store's
writer is slow: the freeze a saturated disk brings.

One daemon on a socket holds AGENTS stored bots. A preloaded shim
(bench/held/slow_sync.c) makes every sync take SLOW_SYNC_MS, as a busy disk
holds a commit, while BUSY bots run turns back to back so the writer always
has a group to commit. Meanwhile, for SECONDS:

- a window polls as Sonny's does, one request after another on one
  connection: `approvals` for a tag, `inspect` of a bot, a page of `bots`;
- a new connection opens every second and waits for its `ready` greeting,
  as an automation's does.

With --scripts N, N more bots run code-mode scripts back to back, each
served by its own client as Sonny serves a script: the call is listed
(`tool_calls`), taken, three steps are put to the bot's gates and recorded,
and the call is answered; each of those requests' waits is recorded too.

Each request's wait is recorded; the result has p50, p99 and the most for
each, the turns the busy bots finished, and the store's own counts.

    cc -O2 -shared -fPIC -o .local/slow_sync.so bench/held/slow_sync.c -ldl
    python -m bench.held_reads --binary .local/target/release/agent \\
        --shim .local/slow_sync.so --agents 424 --out result.json
"""
import argparse
import json
import shutil
import socket
import statistics
import sys
import tempfile
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.runtime_client import serve_args  # noqa: E402
from bench.socket_client import Connection  # noqa: E402
from bench.synthetic_model import start  # noqa: E402
from bench.targets import clean_env  # noqa: E402
import subprocess  # noqa: E402


def daemon(binary, store, url, sock, env):
    process = subprocess.Popen([str(binary), *serve_args(store, url), '--socket', str(sock)],
                               stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL, start_new_session=True, env=env)
    deadline = time.monotonic() + 30
    while True:
        try:
            return process, Connection(sock, retain_durable=False)
        except (FileNotFoundError, ConnectionRefusedError):
            if process.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError('daemon startup failed')
            time.sleep(.01)


def stop(process, control):
    control.request('shutdown', grace_ms=0)
    control.close()
    process.wait(timeout=60)


def summary(waits):
    if not waits:
        return {'count': 0}
    waits = sorted(waits)
    return {'count': len(waits), 'p50_ms': round(statistics.median(waits) * 1000, 1),
            'p99_ms': round(waits[min(len(waits) - 1, int(len(waits) * .99))] * 1000, 1),
            'max_ms': round(waits[-1] * 1000, 1)}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', required=True)
    parser.add_argument('--shim', required=True)
    parser.add_argument('--agents', type=int, default=424)
    parser.add_argument('--busy', type=int, default=30)
    parser.add_argument('--gated', type=int, default=5)
    parser.add_argument('--sync-ms', type=int, default=300)
    parser.add_argument('--seconds', type=float, default=20)
    parser.add_argument('--window', action=argparse.BooleanOptionalAction, default=True)
    parser.add_argument('--scripts', type=int, default=0)
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    server, url = start()
    work = Path(tempfile.mkdtemp(prefix='held-', dir='/tmp'))
    store, sock = work / 'state.sqlite', work / 'd.sock'
    folder = work / 'folder'
    folder.mkdir()
    process = None
    try:
        # Seeded on a fast disk.
        process, control = daemon(args.binary, store, url, sock, clean_env())
        for n in range(args.agents):
            gated = n < args.gated
            extra = {'tools': ['shell'], 'approve': ['shell'], 'approver': 'manual'} if gated else {}
            created = control.request('create', bot=f'b{n}', workspace=str(folder), **extra)
            assert 'result' in created, created
        for n in range(args.scripts):
            script = {'name': 'exec', 'description': 'Run a script.',
                      'parameters': {'type': 'object', 'properties': {}},
                      'answered_by': f'h{n}', 'steps': True}
            created = control.request('create', bot=f's{n}', workspace=str(folder), declare=[script])
            assert 'result' in created, created
        stop(process, control)
        # Then held: every sync waits.
        env = {**clean_env(), 'LD_PRELOAD': str(Path(args.shim).resolve()),
               'SLOW_SYNC_MS': str(args.sync_ms)}
        process, control = daemon(args.binary, store, url, sock, env)
        # Each gated bot asks once, and waits.
        for n in range(args.gated):
            control.request('submit', bot=f'b{n}', request_id=f'g{n}', prompt='shell:true')
        stopping = threading.Event()
        finished = [0]
        failures = []

        def refused_busy(submitted):
            # Only a bot still ending its last turn is tried again.
            if 'result' in submitted:
                return False
            error = submitted.get('error')
            assert (error.get('code') if isinstance(error, dict) else error) == 'bot_busy', submitted
            return True

        def guarded(work, *args):
            # A worker that fails ends the screen with no measurement.
            def run():
                try:
                    work(*args)
                except Exception as error:
                    failures.append(f'{work.__name__}{args}: {error!r}')
                    stopping.set()
            return threading.Thread(target=run)

        def busy(n):
            connection = Connection(sock, retain_durable=False)
            bot, turn = f'b{args.gated + n}', 0
            while not stopping.is_set():
                turn += 1
                submitted = connection.request('submit', bot=bot, request_id=f'r{turn}', prompt='delay:50')
                if refused_busy(submitted):
                    time.sleep(.05)
                    continue
                handle = f'{bot}/{submitted["result"]["message"]}'
                ended = connection.request('wait', handles=[handle], timeout_ms=60_000)
                found = ended['result']['results'][handle]
                assert not found.get('pending') and found.get('status') == 'completed', found
                finished[0] += 1
            connection.close()

        waits = {'approvals': [], 'inspect': [], 'bots': [], 'ready': [],
                 **({op: [] for op in ('tool_calls', 'take_call', 'approve_step', 'step_result',
                                       'tool_result')} if args.scripts else {})}
        scripts = [0]

        def timed(connection, op, **params):
            began = time.monotonic()
            answered = connection.request(op, **params)
            waits[op].append(time.monotonic() - began)
            return answered

        def script(n):
            connection = Connection(sock, retain_durable=False)
            bot, tag, turn = f's{n}', f'h{n}', 0
            while not stopping.is_set():
                turn += 1
                submitted = connection.request('submit', bot=bot, request_id=f'r{turn}', prompt='call:exec')
                if refused_busy(submitted):
                    time.sleep(.05)
                    continue
                call = None
                while call is None and not stopping.is_set():
                    listed = timed(connection, 'tool_calls', tag=tag)['result']['calls']
                    call = next((c for c in listed if c['bot'] == bot), None)
                    if call is None:
                        time.sleep(.01)
                if call is None:
                    break
                at = dict(bot=bot, turn=call['turn'], call_id=call['call_id'])
                assert 'result' in timed(connection, 'take_call', **at)
                for step in ('1', '2', '3'):
                    stepped = timed(connection, 'approve_step', **at, step=step, name='mcp__docs__read',
                                    arguments={'path': step})
                    assert stepped.get('result', {}).get('decision') == 'allow', stepped
                    assert 'result' in timed(connection, 'step_result', **at, step=step)
                assert 'result' in timed(connection, 'tool_result', **at, output='done')
                handle = f'{bot}/{submitted["result"]["message"]}'
                ended = connection.request('wait', handles=[handle], timeout_ms=60_000)
                found = ended['result']['results'][handle]
                assert not found.get('pending') and found.get('status') == 'completed', found
                scripts[0] += 1
            connection.close()

        def window():
            connection = Connection(sock, retain_durable=False)
            reads = [('approvals', {'tag': 'manual'}), ('inspect', {'bot': f'b{args.agents - 1}'}),
                     ('bots', {'limit': 64})]
            while not stopping.is_set():
                for op, params in reads:
                    began = time.monotonic()
                    answered = connection.request(op, **params)
                    assert 'result' in answered, answered
                    waits[op].append(time.monotonic() - began)
                time.sleep(.07)
            connection.close()

        def connects():
            while not stopping.is_set():
                began = time.monotonic()
                connection = Connection(sock, retain_durable=False)
                waits['ready'].append(time.monotonic() - began)
                connection.close()
                stopping.wait(1)

        threads = [guarded(busy, n) for n in range(args.busy)]
        threads += [guarded(script, n) for n in range(args.scripts)]
        time.sleep(1)
        threads += [guarded(window)] if args.window else []
        threads += [guarded(connects)]
        for thread in threads:
            thread.start()
        time.sleep(args.seconds)
        stopping.set()
        for thread in threads:
            thread.join(timeout=120)
        if failures:
            raise RuntimeError('workers failed: ' + '; '.join(failures))
        if args.scripts and not scripts[0]:
            raise RuntimeError('no script finished')
        stats = control.request('stats')['result']
        storage = stats.get('store', stats.get('storage', {}))
        result = {'agents': args.agents, 'busy': args.busy, 'sync_ms': args.sync_ms,
                  'seconds': args.seconds, 'turns_finished': finished[0],
                  'scripts': args.scripts, 'scripts_finished': scripts[0],
                  **{op: summary(w) for op, w in waits.items()},
                  'loop_held_ms': {op: [row['held_ms'], row['slowest_ms']]
                                   for part in ('commands', 'loop')
                                   for op, row in stats.get(part, {}).get('ops', {}).items()
                                   if row['held_ms']},
                  'reads': {op: {k: row[k] for k in ('count', 'queued_ms', 'ran_ms', 'slowest_ms')}
                            for op, row in storage.get('operations', {}).items()
                            if op in ('approvals', 'inspect', 'list', 'identity', 'turn_answer')},
                  # Writes and claims landed, and how long they waited
                  # after landing for the loop, where the daemon says.
                  **{lane: stats[lane] for lane in ('writes', 'claims') if lane in stats},
                  'groups': storage.get('groups', {}).get('count'),
                  'wal_bytes': storage.get('wal_bytes')}
        stop(process, control)
        Path(args.out).write_text(json.dumps(result, indent=1) + '\n')
        print(json.dumps(result))
    finally:
        # A screen that failed leaves no daemon running into the next.
        if process and process.poll() is None:
            process.kill()
            process.wait(timeout=60)
        server.shutdown()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == '__main__':
    main()
