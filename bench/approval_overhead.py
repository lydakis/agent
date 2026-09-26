"""What a tool approval gate costs a turn, against the same turn ungated.

Each arm runs `bots` bots concurrently on a fresh store, `turns` turns each,
every turn one synthetic `shell` call (`true`) and a reply. Arms:

- `ungated`: no gate. Run on `--before` too, when given, to show that a
  daemon with approvals costs an ungated bot nothing.
- `held`: `shell` gated; this client answers `allow` as soon as a call is
  announced, within the hold, so the turn never parks.
- `parked`: the same gate with `--approval-hold-ms 0`; this client answers
  after `turn_waiting`, so every call parks and resumes.

Reported per turn: store jobs and commits by label, storage worker time by
job, daemon CPU, latency from submit to `turn_finished`, event bytes, live
store bytes after a clean close, and the approver's own request and reply
bytes. With `--fsync`, a separate pass per arm counts the daemon's fsync
and fdatasync calls under strace (setup included; CPU and latency from that
pass are not reported).
The approver is this process, so gated latency includes its reaction time;
`waited_ms` on `tool_started` is the daemon's own announce-to-start measure.
"""
import argparse
import http.server
import json
import os
import platform
import socket
import sqlite3
import stat
import tempfile
import threading
import time
from pathlib import Path

import psutil

from .runtime_client import Client
from .synthetic_model import Model
from .targets import file_hash


class Immediate(Model):
    # The model's chunked frames go out at once: without this, Nagle's
    # algorithm and delayed ACKs add tens of milliseconds to every call and
    # bury the difference under measurement.
    def setup(self):
        super().setup()
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)


def start():
    class Server(http.server.ThreadingHTTPServer):
        request_queue_size = 1024
        daemon_threads = True
    server = Server(('127.0.0.1', 0), Immediate)
    server.requests, server.request_bytes = 0, 0
    server.release = threading.Event()
    server.lock, server.flaky_seen = threading.Lock(), set()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f'http://127.0.0.1:{server.server_port}/v1'


def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, max(0, round(len(ordered) * fraction) - 1))]


def live_bytes(path):
    """Bytes of pages in use once the closed store's log is folded in."""
    with sqlite3.connect(path) as conn:
        conn.execute('PRAGMA wal_checkpoint(TRUNCATE)')
        pages, free, size = (conn.execute(f'PRAGMA {p}').fetchone()[0]
                             for p in ('page_count', 'freelist_count', 'page_size'))
    return (pages - free) * size


def run(binary, arm, *, bots, turns, fsync=False):
    server, url = start()
    hold = '0' if arm == 'parked' else '2000'
    with tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parent.parent / '.local') as temp:
        path = Path(temp)
        if fsync:
            wrapper = path / 'traced'
            wrapper.write_text('#!/bin/sh\nexec strace -f -qq --seccomp-bpf -c -e trace=fsync,fdatasync '
                               f'-o {path / "syscalls"} {binary} "$@"\n')
            wrapper.chmod(wrapper.stat().st_mode | stat.S_IXUSR)
            binary = wrapper
        client = Client(binary, path / 'state.sqlite', url, 'shell',
                        extra=() if arm == 'ungated' else ('--approval-hold-ms', hold))
        try:
            process = psutil.Process(client.process.pid)
            names = [f'b{n}' for n in range(bots)]
            gate = {} if arm == 'ungated' else {'approve': ['shell'], 'approver': 'probe'}
            for name in names:
                assert 'result' in client.request('create', bot=name, workspace=str(path), **gate)
            before = client.request('stats')['result']['store']['operations']
            cpu0 = process.cpu_times()
            started, latency, approver_bytes = {}, [], 0
            remaining = {name: turns for name in names}
            pending_submits, announced = {}, {}

            def send(op, **params):
                nonlocal approver_bytes
                client.next_id += 1
                line = json.dumps({'id': client.next_id, 'op': op, **params}) + '\n'
                if op == 'answer':
                    approver_bytes += len(line)
                client.process.stdin.write(line)
                client.process.stdin.flush()
                return client.next_id

            def submit(name):
                remaining[name] -= 1
                pending_submits[send('submit', bot=name, request_id=f'{name}-{remaining[name]}',
                                     prompt='shell:true')] = (name, time.monotonic())

            def answer(message, calls):
                for call in calls:
                    send('answer', bot=message['bot'], turn=message['turn'], call_id=call['call_id'],
                         request=call['request'], decision='allow', by='probe')

            wall0 = time.monotonic()
            for name in names:
                submit(name)
            done = 0
            while done < bots * turns:
                message = client.queue.get(timeout=30)
                assert message is not None, 'daemon exited'
                if message.get('id') in pending_submits:
                    name, at = pending_submits.pop(message['id'])
                    started[message['result']['turn']] = (name, at)
                elif message.get('id') is not None:
                    assert 'error' not in message, message
                    if 'decision' in message.get('result', {}):
                        approver_bytes += len(json.dumps(message, separators=(',', ':'))) + 1
                elif message.get('event') == 'approval_requested':
                    calls = message['data']['calls']
                    if arm == 'held':
                        answer(message, calls)
                    else:
                        announced[message['turn']] = calls
                elif message.get('event') == 'turn_waiting' and message['data'].get('approval'):
                    answer(message, announced.pop(message['turn']))
                elif message.get('event') == 'turn_finished':
                    assert message['data']['status'] == 'completed', message
                    name, at = started.pop(message['turn'])
                    latency.append((message['_received_at'] - at) * 1000)
                    done += 1
                    if remaining[name]:
                        submit(name)
            wall = time.monotonic() - wall0
            cpu1 = process.cpu_times()
            stats = client.request('stats')['result']
            after = stats['store']['operations']
            assert not stats.get('approval_requests')
            count = bots * turns
            jobs = {label: (after[label]['count'] - before.get(label, {}).get('count', 0)) / count
                    for label in after}
            # Time the storage worker and reader spent running each job.
            worker_ms = {label: (after[label]['ran_ms'] - before.get(label, {}).get('ran_ms', 0)) / count
                         for label in after}
            # The two stats reads: the first one's group commit falls inside
            # the window, and the second one's `counts` job does.
            jobs['counts'] -= 1 / count
            jobs['commit'] -= 1 / count
            event_bytes, waited = 0, []
            for name in names:
                cursor = 0
                while True:
                    page = client.request('events', bot=name, after=cursor, limit=256)['result']
                    for event in page['events']:
                        event_bytes += len(json.dumps(event, separators=(',', ':'))) + 1
                        if event['event'] == 'tool_started' and 'approvals' in event['data']:
                            waited.extend(a['waited_ms'] for a in event['data']['approvals'])
                    if not page['events']:
                        break
                    cursor = page['next_cursor']
        finally:
            client.close()
            server.shutdown()
            server.server_close()
        if fsync:
            calls = 0
            for line in (path / 'syscalls').read_text().splitlines():
                fields = line.split()
                if fields and fields[-1] in ('fsync', 'fdatasync'):
                    calls += int(fields[3])
            return {'arm': arm, 'bots': bots, 'turns': turns, 'fsync_per_turn': round(calls / count, 3)}
        store_bytes = live_bytes(path / 'state.sqlite')
    return {
        'arm': arm, 'bots': bots, 'turns': turns,
        'turns_per_s': round(count / wall, 1),
        'latency_ms': {q: round(percentile(latency, f), 2) for q, f in (('p50', .5), ('p95', .95), ('p99', .99))},
        'daemon_cpu_ms_per_turn': round((cpu1.user + cpu1.system - cpu0.user - cpu0.system) * 1000 / count, 3),
        'jobs_per_turn': {k: round(v, 3) for k, v in sorted(jobs.items()) if round(v, 3)},
        'store_ms_per_turn': round(sum(worker_ms.values()), 3),
        'store_ms_by_job': {k: round(v, 3) for k, v in sorted(worker_ms.items()) if round(v, 3)},
        'event_bytes_per_turn': round(event_bytes / count),
        'store_bytes_per_turn': round(store_bytes / count),
        'approver_bytes_per_turn': round(approver_bytes / count),
        'waited_ms': {q: percentile(waited, f) for q, f in (('p50', .5), ('p95', .95))} if waited else None,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--after', type=Path, required=True, help='binary with approvals')
    parser.add_argument('--before', type=Path, help='binary without approvals, for the ungated arm')
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--bots', type=int, default=1)
    parser.add_argument('--turns', type=int, default=200)
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--fsync', action='store_true', help='also count fsyncs per arm under strace')
    parser.add_argument('--arms', default='ungated,held,parked', help='arms on --after, comma separated')
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    arms = [('after', arm) for arm in args.arms.split(',')]
    assert arms and all(arm in ('ungated', 'held', 'parked') for _, arm in arms)
    if args.before:
        arms.insert(0, ('before', 'ungated'))
    binaries = {'after': args.after.resolve(), **({'before': args.before.resolve()} if args.before else {})}
    report = {'schema': 1, 'host': platform.platform(), 'cpus': os.cpu_count(),
              'binary_sha256': {k: file_hash(v) for k, v in binaries.items()},
              'workload': {'bots': args.bots, 'turns': args.turns, 'rounds': args.rounds, 'arms': args.arms},
              'runs': [], 'fsync': []}
    for index in range(args.rounds):
        # Rotate the order so no arm always runs first or last.
        for label, arm in arms[index % len(arms):] + arms[:index % len(arms)]:
            record = {'binary': label, **run(binaries[label], arm, bots=args.bots, turns=args.turns)}
            report['runs'].append(record)
            (args.out / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
            print(json.dumps(record), flush=True)
    if args.fsync:
        for label, arm in arms:
            record = {'binary': label, **run(binaries[label], arm, bots=args.bots, turns=args.turns, fsync=True)}
            report['fsync'].append(record)
            (args.out / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
            print(json.dumps(record), flush=True)


if __name__ == '__main__':
    main()
