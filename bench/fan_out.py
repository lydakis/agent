"""What one bot's live stream costs the daemon as its audience grows.

One bot takes streaming turns back to back against `bench.streaming_model`
(every token its own `text_delta`), while FOLLOWERS socket sessions follow
that same bot and read everything as fast as a socket gives it. With
--slow, one more follower follows the bot and never reads: the daemon
closes it once its queue is full, and the fast followers must not slow
down. Each count of followers gets its own daemon. Reports, over the
measured window, the daemon's CPU per live event published (and the
runtime thread's alone), turns and events per second, and what each fast
follower received.

    .local/venv/bin/python -m bench.fan_out --followers 0,1,32,200 --slow \\
        --out .local/bench/fan-out.json
"""
import argparse
import json
import os
import selectors
import socket
import subprocess
import tempfile
import threading
import time
from pathlib import Path

from .scale_screen import thread_cpu
from .socket_client import Connection
from .targets import clean_env

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent


class Readers:
    """Follower sockets drained by one thread, counting lines, never parsing."""

    def __init__(self):
        self.selector = selectors.DefaultSelector()
        self.lines, self.bytes = {}, {}
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def add(self, name, sock):
        sock.setblocking(False)
        self.lines[name] = self.bytes[name] = 0
        self.selector.register(sock, selectors.EVENT_READ, name)

    def run(self):
        while not self.stop.is_set():
            if not self.selector.get_map():
                time.sleep(.01)
                continue
            for key, _ in self.selector.select(.05):
                try:
                    data = key.fileobj.recv(1 << 20)
                except BlockingIOError:
                    continue
                except OSError:
                    data = b''
                if not data:
                    self.selector.unregister(key.fileobj)
                    continue
                self.lines[key.data] += data.count(b'\n')
                self.bytes[key.data] += len(data)

    def snapshot(self):
        return dict(self.lines), dict(self.bytes)


def follow(path, bot):
    sock = socket.socket(socket.AF_UNIX)
    sock.connect(str(path))
    sock.sendall((json.dumps(dict(id=1, op='follow', bot=bot)) + '\n').encode())
    return sock


def closed(sock):
    """Whether the daemon closed a socket nobody read: drain what it holds
    and see whether it ends."""
    sock.setblocking(False)
    try:
        while True:
            if not sock.recv(1 << 20):
                return True
    except BlockingIOError:
        return False
    except OSError:
        return True


def measure(args, followers):
    work = Path(tempfile.mkdtemp(prefix='fan-', dir='/tmp'))
    sock = work / 'd.sock'
    model = subprocess.Popen(
        [os.sys.executable, str(HERE / 'streaming_model.py'), '--stats', str(work / 'model.json'),
         '--ttft-ms', '0', '--tokens', str(args.tokens), '--tps', '1000000', '--per-delta', '1'],
        stdout=subprocess.PIPE, text=True)
    port = json.loads(model.stdout.readline())['port']
    env = {**clean_env(), 'HOME': str(work / 'home')}
    daemon = subprocess.Popen(
        [args.binary, 'serve', '--store', str(work / 'state.sqlite'), '--socket', str(sock),
         '--provider', f'openai=responses,http://127.0.0.1:{port}/v1'],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        env=env, start_new_session=True)
    readers = Readers()
    sockets = []
    try:
        deadline = time.monotonic() + 10
        while True:
            try:
                control = Connection(sock, retain_durable=False)
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if daemon.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError('daemon did not start')
                time.sleep(.01)
        created = control.request('create', bot='b0', workspace=str(work), model='openai/mock')
        assert 'result' in created, created
        for n in range(followers):
            sockets.append(follow(sock, 'b0'))
            readers.add(f'f{n}', sockets[-1])
        slow = None
        if args.slow:
            slow = socket.socket(socket.AF_UNIX)
            slow.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
            slow.connect(str(sock))
            slow.sendall((json.dumps(dict(id=1, op='follow', bot='b0')) + '\n').encode())
        time.sleep(.5)
        turns = [0]
        measuring = threading.Event()
        stopping = threading.Event()

        # Its own connection: one is read by one thread.
        driving = Connection(sock, retain_durable=False)

        def drive():
            n = 0
            while not stopping.is_set():
                n += 1
                sent = driving.request('submit', bot='b0', request_id=f'r{n}', prompt=f'go {n}')
                assert 'result' in sent, sent
                handle = f'b0/{sent["result"]["message"]}'
                driving.request('wait', handles=[handle], timeout_ms=60_000)
                if measuring.is_set():
                    turns[0] += 1

        driver = threading.Thread(target=drive, daemon=True)
        driver.start()
        time.sleep(args.warmup)
        s0 = control.request('stats')['result']
        t0, cpu0, (l0, b0) = time.monotonic(), thread_cpu(daemon.pid), readers.snapshot()
        measuring.set()
        time.sleep(args.seconds)
        measuring.clear()
        t1, cpu1, (l1, b1) = time.monotonic(), thread_cpu(daemon.pid), readers.snapshot()
        s1 = control.request('stats')['result']
        stopping.set()
        driver.join(timeout=60)
        dt = t1 - t0
        hub = {k: s1['hub'][k] - s0['hub'][k] for k in ('live', 'durable', 'delivered', 'dropped')}
        used = {k: v - cpu0.get(k, 0) for k, v in cpu1.items()}
        total = sum(used.values())
        if args.threads:
            print(json.dumps({k: round(v, 3) for k, v in used.items() if v}), flush=True)
        # Everything but the store's threads: the run loop is the main one.
        runtime = sum(v for k, v in used.items() if not k.startswith('agent-storage'))
        published = hub['live'] + hub['durable']
        got = sorted(l1[k] - l0[k] for k in l1)
        result = dict(
            followers=followers, slow=bool(args.slow), seconds=round(dt, 2),
            turns_per_s=round(turns[0] / dt, 2),
            published_per_s=round(published / dt),
            delivered_per_s=round(hub['delivered'] / dt),
            dropped=hub['dropped'],
            daemon_cpu_pct=round(total / dt * 100, 1),
            run_loop_pct=round(runtime / dt * 100, 1),
            daemon_us_per_published=round(total / max(1, published) * 1e6, 2),
            run_loop_us_per_published=round(runtime / max(1, published) * 1e6, 2),
            daemon_us_per_delivered=round(total / max(1, hub['delivered']) * 1e6, 3),
            follower_lines_per_s_min=round(got[0] / dt) if got else None,
            follower_lines_per_s_max=round(got[-1] / dt) if got else None,
            follower_kib_per_s=round(sum(b1[k] - b0[k] for k in b1) / max(1, len(b1)) / dt / 1024, 1),
        )
        if slow is not None:
            result['slow_closed'] = closed(slow)
        control.request('shutdown')
        daemon.wait(timeout=30)
        return result
    finally:
        readers.stop.set()
        for s in sockets:
            s.close()
        for process in (daemon, model):
            if process.poll() is None:
                process.kill()
                process.wait()


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--binary', default=str(ROOT / '.local/target/release/agent'))
    p.add_argument('--followers', default='0,1,32,200')
    p.add_argument('--slow', action='store_true')
    p.add_argument('--tokens', type=int, default=200)
    p.add_argument('--warmup', type=float, default=3)
    p.add_argument('--seconds', type=float, default=15)
    p.add_argument('--out')
    p.add_argument('--threads', action='store_true')
    args = p.parse_args()
    results = []
    for n in (int(n) for n in args.followers.split(',')):
        results.append(measure(args, n))
        print(json.dumps(results[-1]), flush=True)
    if args.out:
        Path(args.out).write_text(json.dumps(results, indent=2))


if __name__ == '__main__':
    main()
