"""What a turn costs the drive, against what it adds to the store.

Turns one-reply turns one at a time on BOTS bots, first on a fresh store and
then on one already holding PREFILL turns a bot, against a synthetic
Responses model. Reports the bytes the daemon sent toward the disk per turn
(Linux's /proc io `write_bytes`: WAL frames, checkpoints and sidecars), the
store's growth per turn, and the p50 and p99 turn. Linux only.

    .local/venv/bin/python -m bench.disk_writes --out .local/bench/disk-writes
"""
import argparse
import http.server
import json
import threading
import time
from pathlib import Path

import psutil

from .long_history import Model
from .runtime_client import Client


def store_bytes(out):
    return sum(p.stat().st_size for p in out.glob('state.sqlite*'))


def run(binary, out, bots, prefill, turns):
    out.mkdir(parents=True)
    out = out.resolve()
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)
    server.seen, server.daemon_threads = [], True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    client = Client(binary, out / 'state.sqlite', f'http://127.0.0.1:{server.server_port}/v1', 'echo')
    try:
        def turn(bot, request_id):
            reply = client.request('submit', bot=bot, request_id=request_id, prompt='seed ' + 'x' * 200,
                                   workspace=str(out))
            assert 'result' in reply, reply
            sent = reply['result']['message']
            client.receive(lambda m: m.get('event') == 'turn_finished' and m.get('bot') == bot
                           and m.get('turn') == sent, timeout=60)
            client.saved.clear()

        names = [f'b{n}' for n in range(bots)]
        for bot in names:
            client.request('create', bot=bot, workspace=str(out))
        for n in range(prefill):
            for bot in names:
                turn(bot, f'p{n}')
        process = psutil.Process(client.process.pid)
        written, stored, took = process.io_counters().write_bytes, store_bytes(out), []
        for n in range(turns):
            for bot in names:
                began = time.monotonic()
                turn(bot, f'm{n}')
                took.append((time.monotonic() - began) * 1000)
        written = process.io_counters().write_bytes - written
        stored = store_bytes(out) - stored
    finally:
        client.close()
        server.shutdown()
    took.sort()
    measured = len(took)
    return dict(bots=bots, prefill_turns=prefill * bots, measured_turns=measured,
                written_kib_per_turn=round(written / measured / 1024, 1),
                store_growth_kib_per_turn=round(stored / measured / 1024, 2),
                turn_ms=dict(p50=round(took[measured // 2], 1), p99=round(took[int(measured * .99)], 1)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--bots', type=int, default=10)
    parser.add_argument('--prefill', type=int, default=300)
    parser.add_argument('--turns', type=int, default=20)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    rows = [run(args.binary.resolve(), args.out / f'prefill-{n}', args.bots, n, args.turns)
            for n in (0, args.prefill)]
    (args.out / 'result.json').write_text(json.dumps(rows, indent=2) + '\n')
    for row in rows:
        print(json.dumps(row), flush=True)


if __name__ == '__main__':
    main()
