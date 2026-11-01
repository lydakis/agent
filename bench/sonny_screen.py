"""What Sonny costs at fleet scale, as the Perf workflow screens it.

One socket daemon on a streaming synthetic model (bench.lookout_scale's), and
Sonny's three paths that grow with the fleet:

- lookout: `sonny lookout` over 10 leads with S tasks between them, for
  each S in --subscribers. How long until every lead is seated, its CPU per
  thousand streamed deltas while every task runs turns, and how late a lead
  hears that a task's turn ended.
- window: one quiet lead's Sonny, in an 80x24 terminal, while the largest
  fleet's tasks run turns it has nothing to do with. Its CPU, its RSS, and
  what it asks the daemon for and reads back, counted on a socket that
  passes everything through to the daemon.
- sky: `sonny sky --replay 1h` over everything the fleet said, played at
  its fastest speed until the whole hour has played: its CPU and RSS.

A bench that times Sonny on a shared runner only warns; bench.perf_gate
compares two of these runs.

Run from the repo root after `cargo build --release --bin agent --bin sonny`:
    python3 -m bench.sonny_screen --out /tmp/x
"""
import argparse
import asyncio
import fcntl
import json
import os
import pty
import signal
import struct
import termios
import threading
import time
from pathlib import Path
from types import SimpleNamespace

from bench.lookout_scale import Bench, Sampler, cpu_seconds


class Tap:
    """A socket in front of the daemon's that counts what its clients ask
    for, by op, and the bytes the daemon sends back."""

    def __init__(self, daemon, path):
        self.daemon, self.path = str(daemon), str(path)
        self.ops, self.sent, self.read = {}, 0, 0
        self.ready = threading.Event()
        self.loop = asyncio.new_event_loop()
        threading.Thread(target=self.serve, daemon=True).start()
        self.ready.wait(10)

    def serve(self):
        asyncio.set_event_loop(self.loop)

        async def pipe(reader, writer, upward):
            try:
                while line := await reader.readline():
                    if upward:
                        self.sent += len(line)
                        try:
                            op = json.loads(line).get('op', '?')
                        except ValueError:
                            op = '?'
                        self.ops[op] = self.ops.get(op, 0) + 1
                        if os.environ.get('SS_TAP_LOG'):
                            with open(os.environ['SS_TAP_LOG'], 'a') as log:
                                log.write(line.decode(errors='replace')[:300])
                    else:
                        self.read += len(line)
                    writer.write(line)
                    await writer.drain()
            except (ConnectionError, asyncio.LimitOverrunError, ValueError):
                pass
            finally:
                writer.close()

        async def client(reader, writer):
            up_reader, up_writer = await asyncio.open_unix_connection(self.daemon, limit=64 << 20)
            await asyncio.gather(pipe(reader, up_writer, True), pipe(up_reader, writer, False))

        async def main():
            await asyncio.start_unix_server(client, self.path, limit=64 << 20)
            self.ready.set()
            await asyncio.Event().wait()

        self.loop.run_until_complete(main())

    def counts(self):
        return dict(self.ops), self.sent, self.read


class Screen:
    """A program in a pseudo-terminal of a fixed size, its output drained
    and counted so it never blocks on a full terminal."""

    def __init__(self, argv, env, cwd, columns=80, rows=24):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(cwd)
            os.execve(argv[0], argv, env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, columns, 0, 0))
        self.drawn, self.last = 0, b''
        threading.Thread(target=self.drain, daemon=True).start()

    def drain(self):
        while True:
            try:
                chunk = os.read(self.fd, 1 << 16)
            except OSError:
                return
            if not chunk:
                return
            self.drawn += len(chunk)
            self.last = (self.last + chunk)[-4096:]

    def settle(self, quiet=.02, limit=120):
        """Seconds until a half-second passes with under `quiet` CPU seconds."""
        started, last = time.monotonic(), cpu_seconds(self.pid)
        while time.monotonic() - started < limit:
            time.sleep(.5)
            now = cpu_seconds(self.pid)
            if now - last < quiet:
                break
            last = now
        return round(time.monotonic() - started - .5, 2)

    def check(self, when):
        """Refuse the run unless the program is still up and has drawn:
        an exited window costs nothing and would read as a fast one."""
        try:
            ended = os.waitpid(self.pid, os.WNOHANG)[0]
        except ChildProcessError:
            ended = True
        if ended or not self.drawn:
            tail = self.last[-600:].decode(errors='replace')
            state = 'exited' if ended else 'drew nothing'
            raise RuntimeError(f'{state} {when}; last of its screen:\n{tail}')

    def close(self):
        try:
            os.kill(self.pid, signal.SIGTERM)
            for _ in range(50):
                if os.waitpid(self.pid, os.WNOHANG)[0]:
                    break
                time.sleep(.1)
            else:
                os.kill(self.pid, signal.SIGKILL)
                os.waitpid(self.pid, 0)
        except (ProcessLookupError, ChildProcessError):
            pass
        os.close(self.fd)


def lookout(args, subscribers, out):
    """The lookout over 10 leads and `subscribers` tasks; the bench stays
    open, so the window and the sky can run over the same fleet."""
    bench = Bench(SimpleNamespace(
        agent=args.agent, sonny=args.sonny, projects=10, tasks=max(1, subscribers // 10),
        deltas=args.deltas, delta_ms=args.delta_ms, settle=args.settle, judge_ms=0,
        out=str(out / f'lookout-{subscribers}')))
    bench.setup()
    result = {'start': bench.start_lookout('start')}
    since = int(time.time() * 1000)
    load = bench.load(args.load_s)
    notes = bench.note_delays(since)
    deltas = load['submitted_turns'] * args.deltas
    result.update({
        'deltas': deltas,
        'cpu_ms_per_1k_deltas': round(1000 * load['lookout']['cpu_s'] / deltas * 1000, 2) if deltas else None,
        'peak_rss_mib': load['lookout']['peak_rss_mib'],
        'alive': load['lookout_alive'],
        'note_delay_ms_p95': notes['delay_ms_p95'], 'told': notes['told'],
        'turns_ended': notes['turns_ended']})
    return bench, result


def window(args, bench):
    """One quiet lead's Sonny while every task in the fleet runs turns."""
    bench.create(['quiet.lead'])
    tap = Tap(bench.socket, bench.tmp / 'tap.sock')
    env = {**bench.env, 'TERM': 'xterm-256color', 'AGENT_STORE': str(bench.store),
           'AGENT_SOCKET': str(tap.path)}
    screen = Screen([str(bench.sonny), '--store', str(bench.store), '--socket', tap.path,
                     '--agent', 'quiet.lead', '--no-layout', '--no-sound'],
                    env, bench.out)
    try:
        opened = screen.settle()
        # What it reads as it opens is done before the window is counted.
        time.sleep(2)
        screen.check('as the window opened')
        ops, sent, read = tap.counts()
        drawn = screen.drawn
        sampler = Sampler(screen.pid)
        load = bench.load(args.idle_s, with_lookout=False)
        stats = sampler.done()
        screen.check('while the fleet ran')
        after_ops, after_sent, after_read = tap.counts()
        seconds = stats['seconds']
        asked = {op: n - ops.get(op, 0) for op, n in after_ops.items() if n - ops.get(op, 0)}
        return {'open_s': opened, 'seconds': seconds, 'fleet_turns': load['submitted_turns'],
                'cpu_ms_per_s': round(1000 * stats['cpu_s'] / seconds, 2),
                'peak_rss_mib': stats['peak_rss_mib'],
                'requests_per_s': round(sum(asked.values()) / seconds, 2), 'requests': asked,
                'read_kib_per_s': round((after_read - read) / 1024 / seconds, 2),
                'sent_kib_per_s': round((after_sent - sent) / 1024 / seconds, 2),
                'drawn_kib_per_s': round((screen.drawn - drawn) / 1024 / seconds, 2)}
    finally:
        screen.close()


def sky(args, bench):
    """`sonny sky --replay 1h` over everything the fleet said, at its
    fastest speed (512x), until the replay has played the whole hour."""
    env = {**bench.env, 'TERM': 'xterm-256color'}
    screen = Screen([str(bench.sonny), 'sky', '--replay', '1h', '--store', str(bench.store),
                     '--socket', str(bench.socket)], env, bench.out, columns=160, rows=48)
    try:
        sampler = Sampler(screen.pid)
        time.sleep(1)
        screen.check('as the replay opened')
        # 8x, doubled six times.
        for _ in range(6):
            os.write(screen.fd, b']')
            time.sleep(.05)
        time.sleep(3600 / 512 + 2)
        stats = sampler.done()
        screen.check('during the replay')
        (bench.out / 'sky.screen').write_bytes(screen.last)
        return {'cpu_s': stats['cpu_s'], 'peak_rss_mib': stats['peak_rss_mib'],
                'seconds': stats['seconds'], 'drawn_kib': round(screen.drawn / 1024, 1)}
    finally:
        screen.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--agent')
    parser.add_argument('--sonny')
    parser.add_argument('--subscribers', default='10,100,1000')
    parser.add_argument('--deltas', type=int, default=20)
    parser.add_argument('--delta-ms', type=int, default=50)
    parser.add_argument('--load-s', type=float, default=10)
    parser.add_argument('--idle-s', type=float, default=15)
    # Long enough for a lead's note of the last turns to start.
    parser.add_argument('--settle', type=float, default=10)
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    sizes = [int(n) for n in args.subscribers.split(',')]
    run = {'lookout': {}}
    for size in sizes:
        bench, run['lookout'][str(size)] = lookout(args, size, out)
        try:
            if size == max(sizes):
                run['window'] = window(args, bench)
                run['sky'] = sky(args, bench)
        finally:
            bench.close()
    print(json.dumps(run, indent=1))
    (out / 'result.json').write_text(json.dumps({'runs': [run]}, indent=1))


if __name__ == '__main__':
    main()
