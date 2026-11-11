"""Thousands of waits giving up at once: how long the daemon stalls.

One socket daemon, synthetic provider, nothing spent. Every prompt is one
the synthetic model holds, so each turn stays running or queued, pending. `bots` bots each get one
turn, and clients send one `wait` per turn (each on its own bot's turn, as a
lead waits on its tasks), all with the same timeout, pipelined over a few
connections so they come due together. Beside them, one prober times a cheap
request (`stats`) in a loop. Reported: the slowest probe while the waits
give up (the daemon's stall), the time from the first deadline to the last
answer, and daemon CPU from just before the deadline to the last answer.

    .local/venv/bin/python -m bench.parked_waits --binary .local/target/release/agent --bots 4000
"""
import argparse
import json
import socket
import tempfile
import threading
import time
from pathlib import Path
import subprocess

import psutil

from .runtime_client import serve_args
from .socket_client import Connection
from .synthetic_model import start
from .targets import clean_env


def pipelined(path, requests):
    """Send every request on one connection without waiting, then read
    every answer; the time each answer arrived, by id."""
    sock = socket.socket(socket.AF_UNIX)
    sock.connect(str(path))
    reader = sock.makefile('r')
    json.loads(reader.readline())  # ready
    answered = {}

    def read():
        for line in reader:
            event = json.loads(line)
            if 'id' in event:
                answered[event['id']] = time.monotonic()
                if len(answered) == len(requests):
                    return
    worker = threading.Thread(target=read, daemon=True)
    worker.start()
    sock.sendall(''.join(json.dumps(r) + '\n' for r in requests).encode())
    return sock, worker, answered


def run(binary, bots, timeout_ms, connections):
    server, url = start()
    directory = tempfile.TemporaryDirectory(prefix='pw-', dir='/tmp')
    path = Path(directory.name) / 'd.sock'
    process = subprocess.Popen(
        [str(binary), *serve_args(Path(directory.name) / 'state.sqlite', url,
                                 extra=('--max-active', '8', '--max-pending', '0', '--max-pending-bytes', '0')),
         '--socket', str(path)],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        start_new_session=True, env=clean_env())
    deadline = time.monotonic() + 5
    while True:
        try:
            control = Connection(path)
            break
        except (FileNotFoundError, ConnectionRefusedError):
            if time.monotonic() > deadline:
                raise
            time.sleep(.01)
    try:
        turns = []
        workspace = Path(directory.name) / 'workspace'
        workspace.mkdir()
        for n in range(bots):
            bot = f'b{n}'
            assert 'result' in control.request('create', bot=bot, workspace=str(workspace)), bot
            response = control.request('submit', bot=bot, prompt='hold: work', request_id=f'r{n}', delivery='queue')
            assert 'result' in response, response
            turns.append(f"{bot}/{response['result']['message']}")
        # Every wait sent first, then the clock: their deadlines fall within
        # the time it takes to send them.
        share = [[] for _ in range(connections)]
        for n, handle in enumerate(turns):
            share[n % connections].append(
                dict(id=n + 1, op='wait', handles=[handle], timeout_ms=timeout_ms))
        daemon = psutil.Process(process.pid)
        sent = time.monotonic()
        waits = [pipelined(path, requests) for requests in share]
        probe = Connection(path)
        probes, cpu_due = [], None
        due = sent + timeout_ms / 1000
        while any(len(answered) < len(requests) for (_, _, answered), requests in zip(waits, share)):
            started = time.monotonic()
            if cpu_due is None and started >= due - .1:
                cpu_due = daemon.cpu_times()
            probe.request('stats')
            probes.append((started, time.monotonic() - started))
            if started - sent > timeout_ms / 1000 + 120:
                raise RuntimeError('waits never answered')
        last = max(max(answered.values()) for _, _, answered in waits)
        cpu = daemon.cpu_times()
        # The window from the first deadline to the last answer.
        during = [took for at, took in probes if due - .05 <= at <= last]
        before = [took for at, took in probes if at < due - .05]
        for sock, worker, _ in waits:
            sock.close()
        probe.close()
        return {
            'bots': bots,
            'drain_ms': round((last - due) * 1000, 1),
            'stall_max_ms': round(max(during, default=0) * 1000, 1),
            'probe_p50_before_ms': round(sorted(before)[len(before) // 2] * 1000, 2) if before else None,
            'probes_during': len(during),
            'drain_cpu_ms': round((cpu.user + cpu.system - cpu_due.user - cpu_due.system) * 1000),
        }
    finally:
        server.release.set()
        try:
            control.request('shutdown')
            process.wait(timeout=10)
        except Exception:
            process.kill()
        control.close()
        server.shutdown()
        directory.cleanup()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', required=True, action='append')
    parser.add_argument('--bots', type=int, action='append', required=True)
    parser.add_argument('--timeout-ms', type=int, default=15000)
    parser.add_argument('--connections', type=int, default=8)
    parser.add_argument('--repeat', type=int, default=1)
    args = parser.parse_args()
    for _ in range(args.repeat):
        for bots in args.bots:
            for binary in args.binary:
                result = run(binary, bots, args.timeout_ms, args.connections)
                print(json.dumps({'binary': binary, **result}), flush=True)


if __name__ == '__main__':
    main()
