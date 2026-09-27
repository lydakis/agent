"""What a tool approval gate costs a turn, against the same turn ungated.

Each arm runs `bots` bots concurrently on a fresh store, `turns` turns each,
every turn one synthetic `shell` call (`true`) and a reply. Arms:

- `ungated`: no gate. Run on `--before` too, when given, to show that a
  daemon with approvals costs an ungated bot nothing.
- `held`: `shell` gated; this client answers `allow` as soon as a call is
  announced, within the hold, so the turn never parks.
- `parked`: the same gate with an `approval_hold_ms` of 0; this client answers
  after `turn_waiting`, so every call parks and resumes.

Over a Unix socket instead of stdio, with the approver a process of its own:

- `socket`: the `held` arm with this client answering over the socket, the
  baseline for the two below.
- `jev`: `agent approver` answers, asking a stand-in for Jev's API that
  judges every call low at once.
- `judge`: `agent approver` answers with the synthetic model as a general
  judge through the daemon (a fork, a turn, and a deletion per round),
  judging every call low at once.

Their records add the approver's CPU per turn and the judge time it reports.

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
import queue
import socket
import sqlite3
import stat
import subprocess
import tempfile
import threading
import time
from pathlib import Path

import psutil

from .runtime_client import Client, serve_args
from .socket_client import Connection
from .synthetic_model import Model
from .targets import clean_env, file_hash


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


class Jev(http.server.BaseHTTPRequestHandler):
    """A stand-in for Jev's API that judges every question low at once."""
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def setup(self):
        super().setup()
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        data = json.dumps({'answers': {id: {'type': 'noul', 'noul': 0.05} for id in body['questions']},
                           'usage': {'input_tokens': 0}}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)


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
    hold = 0 if arm == 'parked' else 2000
    with tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parent.parent / '.local') as temp:
        path = Path(temp)
        if fsync:
            wrapper = path / 'traced'
            wrapper.write_text('#!/bin/sh\nexec strace -f -qq --seccomp-bpf -c -e trace=fsync,fdatasync '
                               f'-o {path / "syscalls"} {binary} "$@"\n')
            wrapper.chmod(wrapper.stat().st_mode | stat.S_IXUSR)
            binary = wrapper
        client = Client(binary, path / 'state.sqlite', url, 'shell',
                        settings=None if arm == 'ungated' else {'approval_hold_ms': hold})
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
                    assert 'error' not in message, message
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


def run_socket(binary, arm, *, bots, turns):
    """The `held` workload over a Unix socket, answered by this client
    (`socket`) or by `agent approver` (`jev`, `judge`)."""
    server, url = start()
    jev = None
    with tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parent.parent / '.local') as temp, \
            tempfile.TemporaryDirectory(prefix='agent-bench-', dir='/tmp') as short:
        path, sock = Path(temp), Path(short) / 'daemon.sock'
        daemon = subprocess.Popen(
            [str(binary), *serve_args(path / 'state.sqlite', url),
             '--socket', str(sock)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True, env=clean_env())
        approver, judged, control = None, queue.Queue(), None
        try:
            deadline = time.monotonic() + 5
            while not sock.exists():
                assert daemon.poll() is None and time.monotonic() < deadline, 'daemon startup failed'
                time.sleep(.01)
            control = Connection(sock, retain_durable=False)
            control.tools = ['shell']
            tag = 'probe' if arm == 'socket' else 'auto'
            names = [f'b{n}' for n in range(bots)]
            for name in names:
                assert 'result' in control.request('create', bot=name, workspace=str(path),
                                                   approve=['shell'], approver=tag,
                                                   settings={'approval_hold_ms': 2000})
            if arm != 'socket':
                env, args = clean_env(), [str(binary), 'approver', '--store', str(path / 'state.sqlite'),
                                         '--socket', str(sock)]
                if arm == 'jev':
                    jev = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Jev)
                    jev.daemon_threads = True
                    threading.Thread(target=jev.serve_forever, daemon=True).start()
                    env['TYPESAFE_API_KEY'] = 'synthetic-judge-key'
                    args += ['--judge-url', f'http://127.0.0.1:{jev.server_port}']
                else:
                    args += ['--judge', 'openai/synthetic-model']
                approver = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                            text=True, env=env)
                threading.Thread(target=lambda: [judged.put(json.loads(line)) for line in approver.stdout],
                                 daemon=True).start()
                assert judged.get(timeout=10)['event'] == 'serving'
            assert 'result' in control.request('follow', bot='*', after=0)
            before = control.request('stats')['result']['store']['operations']
            processes = {'daemon': psutil.Process(daemon.pid)}
            if approver:
                processes['approver'] = psutil.Process(approver.pid)
            cpu0 = {k: p.cpu_times() for k, p in processes.items()}
            started, latency = {}, []
            remaining = {name: turns for name in names}
            pending_submits = {}

            def send(op, **params):
                control.next_id += 1
                control.socket.sendall((json.dumps({'id': control.next_id, 'op': op, **params}) + '\n').encode())
                return control.next_id

            def submit(name):
                remaining[name] -= 1
                pending_submits[send('submit', bot=name, request_id=f'{name}-{remaining[name]}',
                                     prompt='shell:true')] = (name, time.monotonic())

            wall0 = time.monotonic()
            for name in names:
                submit(name)
            done = 0
            while done < bots * turns:
                message = control.queue.get(timeout=30)
                assert message is not None, 'daemon closed the connection'
                if message.get('id') in pending_submits:
                    assert 'error' not in message, message
                    name, at = pending_submits.pop(message['id'])
                    started[message['result']['turn']] = (name, at)
                elif message.get('id') is not None:
                    assert 'error' not in message, message
                elif message.get('bot') not in remaining:
                    continue  # the judge's own bots
                elif message.get('event') == 'approval_requested' and arm == 'socket':
                    for call in message['data']['calls']:
                        send('answer', bot=message['bot'], turn=message['turn'], call_id=call['call_id'],
                             request=call['request'], decision='allow', by='probe')
                elif message.get('event') == 'turn_finished' and message['turn'] in started:
                    assert message['data']['status'] == 'completed', message
                    name, at = started.pop(message['turn'])
                    latency.append((message['_received_at'] - at) * 1000)
                    done += 1
                    if remaining[name]:
                        submit(name)
            wall = time.monotonic() - wall0
            cpu = {k: (p.cpu_times().user + p.cpu_times().system - cpu0[k].user - cpu0[k].system)
                   for k, p in processes.items()}
            time.sleep(.2)  # the last forks' deletions
            after = control.request('stats')['result']['store']['operations']
        finally:
            if control:
                control.close()
            if approver:
                approver.kill()
                approver.wait()
                approver.stdout.close()
            daemon.kill()
            daemon.wait()
            server.shutdown()
            server.server_close()
            if jev:
                jev.shutdown()
                jev.server_close()
    count = bots * turns
    lines = []
    while not judged.empty():
        lines.append(judged.get())
    judge_ms = [line['judge_ms'] for line in lines if line.get('event') == 'judged']
    assert arm == 'socket' or all(c['decision'] == 'allow' for line in lines if line.get('event') == 'judged'
                                  for c in line['calls']), lines
    jobs = {label: (after[label]['count'] - before.get(label, {}).get('count', 0)) / count for label in after}
    worker_ms = {label: (after[label]['ran_ms'] - before.get(label, {}).get('ran_ms', 0)) / count
                 for label in after}
    return {
        'arm': arm, 'bots': bots, 'turns': turns,
        'turns_per_s': round(count / wall, 1),
        'latency_ms': {q: round(percentile(latency, f), 2) for q, f in (('p50', .5), ('p95', .95), ('p99', .99))},
        'daemon_cpu_ms_per_turn': round(cpu['daemon'] * 1000 / count, 3),
        'approver_cpu_ms_per_turn': round(cpu['approver'] * 1000 / count, 3) if 'approver' in cpu else None,
        'judge_ms': {q: percentile(judge_ms, f) for q, f in (('p50', .5), ('p95', .95))} if judge_ms else None,
        'jobs_per_turn': {k: round(v, 3) for k, v in sorted(jobs.items()) if round(v, 3)},
        'store_ms_per_turn': round(sum(worker_ms.values()), 3),
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
    assert arms and all(arm in ('ungated', 'held', 'parked', 'socket', 'jev', 'judge') for _, arm in arms)
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
            measure = run_socket if arm in ('socket', 'jev', 'judge') else run
            record = {'binary': label, **measure(binaries[label], arm, bots=args.bots, turns=args.turns)}
            report['runs'].append(record)
            (args.out / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
            print(json.dumps(record), flush=True)
    if args.fsync:
        for label, arm in [(label, arm) for label, arm in arms if arm in ('ungated', 'held', 'parked')]:
            record = {'binary': label, **run(binaries[label], arm, bots=args.bots, turns=args.turns, fsync=True)}
            report['fsync'].append(record)
            (args.out / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
            print(json.dumps(record), flush=True)


if __name__ == '__main__':
    main()
