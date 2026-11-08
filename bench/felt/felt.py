"""Felt cost on macOS: process launches, file opens, stats and connects per
minute, by process, for one Sonny window on its own daemon (a synthetic
model, a scratch HOME, nothing of the person's touched), through phases:

  idle    the window open on its lead, nothing running
  busy    the window's lead runs turns back to back (streamed text, then a
          shell call), as a lead at work does
  fleet   the window idle while another lead's tasks run turns
  source  the window idle, its lead subscribed to a scripted source

Every process Sonny and the daemon start (the lookout, the tool host, panes)
carries count.dylib (DYLD_INSERT_LIBRARIES), which logs each call to
FELT_LOG/<pid>.log; this script splits the lines by phase and process.

Run from a checkout of the version to measure, after
`cargo build --release --workspace --bins` and
`clang -O2 -dynamiclib -o count.dylib count.c`:
    python3 felt.py --agent target/release/agent --sonny target/release/sonny \
        --dylib count.dylib --out /tmp/felt-090 --seconds 60
"""
import argparse
import collections
import json
import os
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import termios
import fcntl
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent


class Conn:
    def __init__(self, path):
        import socket
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(str(path))
        self.reader = self.sock.makefile('r')
        self.next_id = 0
        self.lock = threading.Lock()

    def request(self, op, **params):
        with self.lock:
            self.next_id += 1
            want = self.next_id
            self.sock.sendall((json.dumps({'id': want, 'op': op, **params}) + '\n').encode())
            for line in self.reader:
                message = json.loads(line)
                if message.get('id') == want and ('result' in message or 'error' in message):
                    return message


def cpu_rss(pids):
    """CPU seconds and RSS MiB of each live pid, from ps."""
    if not pids:
        return {}
    out = subprocess.run(['ps', '-o', 'pid=,cputime=,rss=', '-p', ','.join(map(str, pids))],
                         capture_output=True, text=True).stdout
    found = {}
    for line in out.splitlines():
        pid, cpu, rss = line.split()
        parts = cpu.replace('-', ':').split(':')
        seconds = 0.0
        for part in parts:
            seconds = seconds * 60 + float(part)
        found[int(pid)] = (seconds, int(rss) / 1024)
    return found


class Window:
    """A Sonny in a pseudo-terminal, its output read and thrown away."""

    def __init__(self, argv, env, cwd, columns=120, rows=40):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(cwd)
            os.execve(argv[0], argv, env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, columns, 0, 0))
        self.bytes = 0
        self.want, self.seen = None, threading.Event()
        self.stop = threading.Event()
        threading.Thread(target=self.drain, daemon=True).start()

    def drain(self):
        while not self.stop.is_set():
            ready, _, _ = select.select([self.fd], [], [], .2)
            if ready:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    return
                if not data:
                    return
                self.bytes += len(data)
                if self.want is not None and self.want in data:
                    self.seen.set()
                # Answer the terminal queries a real terminal would.
                if b'\x1b[6n' in data:
                    os.write(self.fd, b'\x1b[1;1R')
                if b'\x1b[c' in data:
                    os.write(self.fd, b'\x1b[?62;22c')

    def probe(self, count=20, gap=.5):
        """Key to echo: type `q`, time until a write holding it comes
        back, then take it out again (unmeasured)."""
        times = []
        for _ in range(count):
            self.seen.clear()
            self.want = b'q'
            at = time.perf_counter()
            os.write(self.fd, b'q')
            if self.seen.wait(2):
                times.append(round((time.perf_counter() - at) * 1000, 2))
            self.want = None
            os.write(self.fd, b'\x7f')
            time.sleep(gap)
        return times

    def close(self):
        self.stop.set()
        try:
            os.kill(self.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--agent', required=True)
    parser.add_argument('--sonny', required=True)
    parser.add_argument('--dylib', required=True)
    parser.add_argument('--out', required=True)
    parser.add_argument('--seconds', type=float, default=60)
    parser.add_argument('--tasks', type=int, default=10)
    parser.add_argument('--phases', default='idle,busy,fleet,source')
    parser.add_argument('--repo', default='.', help='checkout holding bench/ (the synthetic model)')
    args = parser.parse_args()
    for name in ('agent', 'sonny', 'dylib', 'repo'):
        setattr(args, name, str(Path(getattr(args, name)).resolve()))

    out = Path(args.out).resolve()
    if out.exists():
        shutil.rmtree(out)
    logs, home, ws = out / 'logs', out / 'home', out / 'ws'
    for d in (logs, home / '.agent', ws):
        d.mkdir(parents=True)
    subprocess.run(['git', 'init', '-q', '-b', 'main', str(ws)], check=True)
    (ws / 'README').write_text('felt\n')
    store = out / 'state.sqlite'
    sock = Path(f'/tmp/felt-{os.getpid()}.sock')

    port_file = out / 'port'
    model = subprocess.Popen([sys.executable, '-m', 'bench.lookout_scale', '--model', str(port_file),
                              '--judge-ms', '50'], cwd=args.repo, start_new_session=True)
    while not port_file.exists() or not port_file.read_text():
        time.sleep(.05)
    url = f'http://127.0.0.1:{port_file.read_text()}/v1'

    env = {'PATH': os.environ['PATH'], 'HOME': str(home), 'TERM': 'xterm-256color',
           'LANG': 'en_US.UTF-8', 'FELT_LOG': str(logs),
           ('DYLD_INSERT_LIBRARIES' if sys.platform == 'darwin' else 'LD_PRELOAD'): str(Path(args.dylib).resolve())}
    daemon = subprocess.Popen([args.agent, 'serve', '--store', str(store), '--provider',
                               f'openai=responses,{url}', '--socket', str(sock)],
                              stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                              stderr=open(out / 'daemon.err', 'w'), env=env, start_new_session=True)
    try:
        run(args, out, logs, home, ws, store, sock, env)
    finally:
        for proc in (daemon, model):
            proc.terminate()
        let_go_jobs(home)
    report(out)


def let_go_jobs(home):
    """A Sonny on a scratch HOME still schedules into the person's own
    launchd domain (or systemd user manager): let go of every job whose
    file it wrote under the scratch HOME, and of nothing else."""
    if sys.platform == 'darwin':
        for plist in (home / 'Library' / 'LaunchAgents').glob('*.plist'):
            subprocess.run(['launchctl', 'bootout', f'gui/{os.getuid()}/{plist.stem}'],
                           capture_output=True)
            print(f'let go of launchd job {plist.stem}')
    else:
        units = home / '.config' / 'systemd' / 'user'
        for unit in units.glob('*.service') if units.is_dir() else []:
            subprocess.run(['systemctl', '--user', 'disable', '--now', unit.name], capture_output=True)
            print(f'let go of systemd unit {unit.name}')


def run(args, out, logs, home, ws, store, sock, env):
    deadline = time.monotonic() + 15
    while True:
        try:
            control = Conn(sock)
            break
        except (FileNotFoundError, ConnectionRefusedError):
            if time.monotonic() > deadline:
                raise
            time.sleep(.05)

    def create(name, tools):
        made = control.request('create', bot=name, workspace=str(ws), model='openai/synthetic-model',
                               instructions='Felt agent.', tools=tools)
        assert 'error' not in made, made

    create('ws.lead', ['echo', 'shell'])
    create('fleet.lead', ['echo'])
    for t in range(args.tasks):
        create(f'fleet.t{t}', ['echo'])
    control.request('submit', bot='ws.lead', request_id='first', prompt='hi', delivery='reject')
    time.sleep(2)

    window = Window([args.sonny, '--store', str(store), '--socket', str(sock), '--agent', 'ws.lead',
                     '--no-sound', '--no-layout'], env, ws)
    time.sleep(8)  # opened, lookout started, first reads done

    phases = []
    stop = threading.Event()

    def busy_loop(bots, prompts):
        n = 0
        while not stop.is_set():
            for bot in bots:
                prompt = prompts[n % len(prompts)]
                control.request('submit', bot=bot, request_id=f'r{time.time_ns()}', prompt=prompt,
                                delivery='queue')
            n += 1
            # One at a time per bot: wait for the daemon to drain them.
            while not stop.is_set():
                stats = control.request('stats')['result']
                if stats.get('active_turns', 0) + stats.get('queued_turns', 0) == 0:
                    break
                time.sleep(.1)

    for phase in args.phases.split(','):
        started = time.time()
        stop.clear()
        worker = None
        if phase == 'busy':
            worker = threading.Thread(target=busy_loop, args=(['ws.lead'], ['stream:40:25', 'shell:true']))
        elif phase == 'fleet':
            worker = threading.Thread(target=busy_loop, args=(
                [f'fleet.t{t}' for t in range(args.tasks)], ['stream:20:50']))
        elif phase == 'source':
            # A scripted source, as a poller is written: print what is new,
            # exit. The lookout reads its settings every 15 s.
            (home / '.agent' / 'config.toml').write_text(
                '[sonny.sources.ticker]\n'
                'command = "date +\'{\\"id\\":\\"%s\\",\\"text\\":\\"tick %s\\"}\'"\n'
                'every = "10s"\n')
            folder = Path(str(store) + '.lookout')
            folder.mkdir(exist_ok=True)
            (folder / 'ws.lead.json').write_text(json.dumps(
                {'subscriptions': [{'name': 'tick', 'sources': ['ticker']}]}))
            time.sleep(16)
        if worker:
            worker.start()
        time.sleep(3)  # the phase's load under way before counting
        started = time.time()
        pids = [int(p.stem) for p in logs.glob('*.log')]
        before = cpu_rss(pids)
        time.sleep(args.seconds)
        pids = [int(p.stem) for p in logs.glob('*.log')]
        after = cpu_rss(pids)
        ended = time.time()
        # Key to echo under the same load, after the counted window: typing
        # notes the window as used, which writes, and is not idle cost.
        echo = window.probe()
        stop.set()
        if worker:
            worker.join(30)
        phases.append({'phase': phase, 'from_ms': int(started * 1000), 'to_ms': int(ended * 1000),
                       'cpu': {pid: round(after[pid][0] - before.get(pid, (0, 0))[0], 3)
                               for pid in after},
                       'rss_mib': {pid: round(after[pid][1], 1) for pid in after},
                       'echo_ms': echo})
        time.sleep(3)  # let the last turn's echoes settle between phases

    window.close()
    (out / 'phases.json').write_text(json.dumps(phases, indent=1))


def label(start_args):
    words = start_args.split()
    if not words:
        return '?'
    name = Path(words[0]).name
    rest = [w for w in words[1:] if not w.startswith('-') and '/' not in w][:1]
    if name == 'sonny' and not rest:
        return 'sonny window'
    if name == 'sonny' and '--embedded' in words:
        return 'sonny pane'
    return ' '.join([name, *rest])


def report(out):
    phases = json.loads((out / 'phases.json').read_text())
    procs = {}
    lines = collections.defaultdict(list)
    for log in (out / 'logs').glob('*.log'):
        pid = int(log.stem)
        for raw in log.read_text(errors='replace').splitlines():
            parts = raw.split('\t')
            if len(parts) < 4:
                continue
            ms, kind, a, b = int(parts[0]), parts[1], parts[2], parts[3]
            if kind == 'start':
                procs[pid] = label(b)
                continue
            lines[pid].append((ms, kind, a, b))
    result = []
    for phase in phases:
        minutes = (phase['to_ms'] - phase['from_ms']) / 60000
        by_label = collections.defaultdict(lambda: collections.Counter())
        paths = collections.defaultdict(lambda: collections.Counter())
        spawned = collections.Counter()
        for pid, calls in lines.items():
            who = procs.get(pid, '?')
            for ms, kind, a, b in calls:
                if phase['from_ms'] <= ms < phase['to_ms']:
                    by_label[who][kind] += 1
                    if kind in ('spawn', 'spawnfail', 'exec', 'fork'):
                        spawned[f'{who} {kind}: {b or a}'.strip()] += 1
                    else:
                        paths[(who, kind)][a] += 1
        cpu = collections.Counter()
        for pid, seconds in phase['cpu'].items():
            cpu[procs.get(int(pid), '?')] += seconds
        rows = []
        for who in sorted(set(by_label) | set(cpu)):
            counts = by_label[who]
            rows.append({'process': who,
                         'per_min': {k: round(v / minutes, 1) for k, v in sorted(counts.items())},
                         'cpu_pct': round(100 * cpu[who] / (minutes * 60), 2),
                         'top': {kind: paths[(who, kind)].most_common(6)
                                 for kind in ('open', 'openw', 'stat', 'connect', 'rename', 'fsync')
                                 if paths[(who, kind)]}})
        echo = sorted(phase.get('echo_ms') or [])
        result.append({'phase': phase['phase'], 'minutes': round(minutes, 2), 'rows': rows,
                       'launches': spawned.most_common(20),
                       'echo_ms': {'n': len(echo), 'p50': echo[len(echo) // 2] if echo else None,
                                   'max': echo[-1] if echo else None}})
    (out / 'report.json').write_text(json.dumps(result, indent=1))
    for phase in result:
        print(f"\n== {phase['phase']} ({phase['minutes']} min)  key->echo {phase['echo_ms']}")
        for row in phase['rows']:
            print(f"  {row['process']:<22} cpu {row['cpu_pct']:>6}%  {row['per_min']}")
        for what, n in phase['launches']:
            print(f"    launch x{n}: {what[:140]}")


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--report':
        report(Path(sys.argv[2]))
    else:
        main()
