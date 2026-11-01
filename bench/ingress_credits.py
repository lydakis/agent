"""What many clients' long request lines cost the daemon and everyone else.

One socket daemon with a few bots. C clients each send one request line of
L bytes at once, a cheap `inspect` padded with spaces, as a fleet of
clients sending long prompts would; a probe session times a short request
every few milliseconds meanwhile. With `--stall-s` every client sends all
of its line but the newline, and ends it only after that long, as a slow
client would. The daemon's peak resident memory is what
the lines it read ahead held, and the probe's latency is how much they
cost a short request behind them.

Run from the repo root after `cargo build --release --bin agent`:
    python3 -m bench.ingress_credits --out /tmp/ic [--max-input-bytes N]
"""
import argparse
import json
import socket
import threading
import time
from pathlib import Path

from bench.fair_queuing import Probe
from bench.lookout_scale import Bench


def peak_rss_kib(pid):
    for line in Path(f'/proc/{pid}/status').read_text().splitlines():
        if line.startswith('VmHWM:'):
            return int(line.split()[1])
    return None


def long_line(path, bot, size, index, failed, finish):
    try:
        with socket.socket(socket.AF_UNIX) as conn:
            conn.connect(str(path))
            reader = conn.makefile('rb')
            reader.readline()  # ready
            line = json.dumps({'id': index, 'op': 'inspect', 'bot': bot}).encode()
            # All of it but its end, which comes once the others have sent
            # theirs, as a slow client's would.
            conn.sendall(line + b' ' * (size - len(line) - 1))
            finish.wait()
            conn.sendall(b'\n')
            reply = json.loads(reader.readline())
            if 'result' not in reply:
                failed.append(reply)
    except Exception as error:  # a client that never got its answer
        failed.append(repr(error))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--clients', type=int, default=500)
    parser.add_argument('--line-bytes', type=int, default=1 << 20)
    parser.add_argument('--gap-ms', type=float, default=5)
    parser.add_argument('--quiet-s', type=float, default=2)
    parser.add_argument('--max-input-bytes', type=int)
    # How long every line waits, all but its newline sent, before it ends.
    parser.add_argument('--stall-s', type=float, default=0)
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    args.projects, args.tasks, args.judge_ms = 1, 2, 0
    args.daemon_flags = (['--max-input-bytes', str(args.max_input_bytes)]
                         if args.max_input_bytes is not None else [])
    bench = Bench(args)
    results = {'clients': args.clients, 'line_bytes': args.line_bytes,
               'max_input_bytes': args.max_input_bytes}
    try:
        bench.setup()
        bot = bench.tasks()[0]
        probe = Probe(bench.socket, bot, args.gap_ms / 1000)
        time.sleep(args.quiet_s)
        results['alone'] = probe.done()
        results['rss_before_kib'] = peak_rss_kib(bench.daemon.pid)
        probe = Probe(bench.socket, bot, args.gap_ms / 1000)
        time.sleep(.2)
        failed, finish = [], threading.Event()
        threads = [threading.Thread(target=long_line,
                                    args=(bench.socket, bot, args.line_bytes, i, failed, finish))
                   for i in range(args.clients)]
        started = time.monotonic()
        for thread in threads:
            thread.start()
        time.sleep(args.stall_s)
        results['stalled_rss_kib'] = peak_rss_kib(bench.daemon.pid)
        finish.set()
        for thread in threads:
            thread.join()
        results['flood_s'] = round(time.monotonic() - started, 2)
        results['probe'] = probe.done()
        results['errors'] = len(failed)
        results['peak_rss_kib'] = peak_rss_kib(bench.daemon.pid)
        if failed:
            raise RuntimeError(f'{len(failed)} long lines failed: {failed[:3]}')
        print(json.dumps(results, indent=1))
        Path(args.out, 'results.json').write_text(json.dumps(results, indent=1))
    finally:
        bench.close()


if __name__ == '__main__':
    main()
