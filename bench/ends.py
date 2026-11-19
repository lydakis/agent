"""The low end and the high end at once: a daemon change must not make
either worse (George, 11-18). Sonny windows run in pseudo-terminals on
their own daemon, a synthetic model, and a scratch HOME.

  low   one window on a folder of AGENTS (10) agents: the window's start
        time, idle CPU and RSS of the window and the daemon, key to
        character, and turn latency (submit to finished, one at a time).
  high  STORED (10,000) agents, WINDOWS windows, one busy project (every
        window in one folder) or spread (each window its own folder),
        while BUSY agents run turns back to back and SCRIPTS agents run
        code-mode scripts as Sonny serves them (list, take, three steps
        put to the gates and recorded, answer): key to character across
        every window, its p99 and the pauses over 100 ms, and the CPU and
        RSS of the daemon and of the windows together. The windows open
        before the fleet starts, or with `--open busy` while it runs, so
        their start is measured under load.

Key to character types `q` into a window and times the first write back
holding it, then takes it out again unmeasured. One poller reads every
window's terminal, so a hundred windows do not mean a hundred readers.

Build both versions first; Sonny is the same for both when the protocol
is:
    python -m bench.ends --agent A --sonny S --end low --out low.json
    python -m bench.ends --agent A --sonny S --end high --windows 100 \\
        --layout spread --seed .local/ends-10k.sqlite --out high.json
`--seed` keeps the stored agents from a first run (created by the daemon
being measured when the file is missing) and copies it for each run.
"""
import argparse
import fcntl
import json
import os
import pty
import select
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import termios
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.socket_client import Connection  # noqa: E402
from bench.synthetic_model import start  # noqa: E402

# What a window draws once it can be typed in: its composer's prompt.
COMPOSER = b'Message'
SCRIPT = {'name': 'exec', 'description': 'Run a script.',
          'parameters': {'type': 'object', 'properties': {}}, 'steps': True}


class Windows:
    """Sonny windows in pseudo-terminals, read by one poller."""

    def __init__(self):
        self.fds, self.pids = {}, {}
        # When each window first drew its composer, and the tail of what
        # it wrote until then, so a word split across reads is found.
        self.ready, self.tails = {}, {}
        self.waiting = {}
        self.lock = threading.Lock()
        self.stop = threading.Event()
        self.poller = threading.Thread(target=self.poll, daemon=True)
        self.poller.start()

    def open(self, name, argv, env, cwd, columns=120, rows=40):
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(cwd)
            os.execve(argv[0], argv, env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, columns, 0, 0))
        with self.lock:
            self.fds[fd] = name
            self.pids[name] = (pid, fd, time.perf_counter())
        return name

    def poll(self):
        while not self.stop.is_set():
            with self.lock:
                fds = list(self.fds)
            if not fds:
                time.sleep(.01)
                continue
            ready, _, _ = select.select(fds, [], [], .05)
            for fd in ready:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    data = b''
                with self.lock:
                    name = self.fds.get(fd)
                    if not data:
                        self.fds.pop(fd, None)
                        continue
                    if name not in self.ready:
                        seen = self.tails.get(fd, b'') + data
                        if COMPOSER in seen:
                            self.ready[name] = time.perf_counter()
                            self.tails.pop(fd, None)
                        else:
                            self.tails[fd] = seen[-64:]
                    waiting = self.waiting.get(fd)
                    if waiting and waiting[0] in data:
                        waiting[1].set()
                # Answer the terminal queries a real terminal would.
                if b'\x1b[6n' in data:
                    os.write(fd, b'\x1b[1;1R')
                if b'\x1b[c' in data:
                    os.write(fd, b'\x1b[?62;22c')

    def drawn(self, name, timeout=30):
        """Seconds from the window's start until it drew its composer."""
        _, _, began = self.pids[name]
        deadline = time.monotonic() + timeout
        while name not in self.ready and time.monotonic() < deadline:
            time.sleep(.005)
        return self.ready[name] - began if name in self.ready else None

    def probe(self, name):
        """Key to character in ms, or None when nothing came back in 2 s."""
        _, fd, _ = self.pids[name]
        seen = threading.Event()
        with self.lock:
            self.waiting[fd] = (b'q', seen)
        at = time.perf_counter()
        os.write(fd, b'q')
        found = seen.wait(2)
        took = (time.perf_counter() - at) * 1000
        with self.lock:
            self.waiting.pop(fd, None)
        os.write(fd, b'\x7f')
        return round(took, 2) if found else None

    def close(self):
        for pid, _, _ in self.pids.values():
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        deadline = time.monotonic() + 10
        for pid, _, _ in self.pids.values():
            while time.monotonic() < deadline and not os.waitpid(pid, os.WNOHANG)[0]:
                time.sleep(.02)
            try:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            except (ProcessLookupError, ChildProcessError):
                pass
        self.stop.set()


def tree(*roots):
    """The `roots` and every process below them, each once, from one scan
    of the process table."""
    children = {}
    for entry in Path('/proc').iterdir():
        if entry.name.isdigit():
            try:
                parent = int((entry / 'stat').read_text().rsplit(')', 1)[1].split()[1])
            except (OSError, IndexError):
                continue
            children.setdefault(parent, []).append(int(entry.name))
    found, todo = set(), list(roots)
    while todo:
        p = todo.pop()
        if p not in found:
            found.add(p)
            todo += children.get(p, [])
    return sorted(found)


def usage(pids):
    """CPU seconds, RSS MiB and wakeups (the times any of their threads was
    put on a CPU, from each thread's schedstat), summed over `pids`."""
    tick, page = os.sysconf('SC_CLK_TCK'), os.sysconf('SC_PAGE_SIZE')
    cpu = rss = wakeups = 0.0
    for pid in pids:
        try:
            fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
            rss += int(Path(f'/proc/{pid}/statm').read_text().split()[1]) * page / 2**20
            cpu += (int(fields[11]) + int(fields[12])) / tick
            for task in Path(f'/proc/{pid}/task').iterdir():
                try:
                    wakeups += int((task / 'schedstat').read_text().split()[2])
                except (OSError, IndexError):
                    pass
        except (OSError, IndexError):
            pass
    return cpu, rss, wakeups


def pss(pids):
    """Proportional set size MiB, summed over `pids`: each shared page
    counted once across them all."""
    total = 0
    for pid in pids:
        try:
            for line in Path(f'/proc/{pid}/smaps_rollup').read_text().splitlines():
                if line.startswith('Pss:'):
                    total += int(line.split()[1])
                    break
        except (OSError, IndexError, ValueError):
            pass
    return total / 1024


def threads(pid):
    """CPU seconds by thread name of process `pid`."""
    tick, found = os.sysconf('SC_CLK_TCK'), {}
    for task in Path(f'/proc/{pid}/task').iterdir():
        try:
            stat = (task / 'stat').read_text()
        except OSError:
            continue
        name = stat[stat.index('(') + 1:stat.rindex(')')]
        fields = stat.rsplit(')', 1)[1].split()
        found[name] = found.get(name, 0) + (int(fields[11]) + int(fields[12])) / tick
    return found


def counters(control):
    """The daemon's requests and loop-held milliseconds by op, so far, and
    the writes and claims landed with how long they waited after landing
    (`behind_ms`) for the loop to take them up, where the daemon says."""
    stats = control.request('stats')['result']
    requests = {op: row['count'] for op, row in stats.get('commands', {}).get('ops', {}).items()}
    held = {}
    for part in ('commands', 'loop'):
        for op, row in stats.get(part, {}).get('ops', {}).items():
            held[op] = held.get(op, 0) + (row.get('held_ms') or 0)
    behind = {f'{lane}_{key}': stats[lane][key] for lane in ('writes', 'claims')
              if lane in stats for key in ('landed', 'behind_ms', 'behind_most_ms')}
    return requests, held, behind


def measured(groups, seconds, daemon=None, control=None):
    """CPU percent and RSS MiB of each group of processes over `seconds`,
    and, with `control`, the daemon's requests and loop-held time over the
    same seconds."""
    marked = counters(control) if control else None
    found_pids = {name: pids() for name, pids in groups.items()}
    began = time.monotonic()
    before = {name: usage(found_pids[name]) for name in groups}
    held = threads(daemon) if daemon else {}
    time.sleep(seconds)
    found_pids = {name: pids() for name, pids in groups.items()}
    elapsed = time.monotonic() - began
    after = {name: usage(found_pids[name]) for name in groups}
    if control:
        now = counters(control)
        found_counts = [{op: count - was.get(op, 0) for op, count in seen.items()
                         if count - was.get(op, 0)} for seen, was in zip(now[:2], marked[:2])]
        # Most is since the daemon started; the rest over these seconds.
        found_behind = {key: value if key.endswith('most_ms') else value - marked[2].get(key, 0)
                        for key, value in now[2].items()}
    # CPU over the time measured, not the time asked for.
    found = {name: {'cpu_pct': round(100 * (after[name][0] - before[name][0]) / elapsed, 2),
                    'rss_mib': round(after[name][1], 1),
                    'wakeups_s': round((after[name][2] - before[name][2]) / elapsed, 1)}
             for name in groups}
    if daemon:
        # The daemon's CPU by thread: its run loop, the writer, the readers.
        found['daemon']['threads_pct'] = {
            name: round(100 * (cpu - held.get(name, 0)) / elapsed, 2)
            for name, cpu in sorted(threads(daemon).items(), key=lambda kv: -kv[1])}
    if control:
        found['requests'], found['loop_held_ms'] = found_counts
        found['behind'] = found_behind
    # Footprint, after the CPU window so reading it costs that window
    # nothing: summed PSS beside summed RSS, so what each window adds is
    # plain once their binary pages are shared.
    for name in groups:
        found[name]['pss_mib'] = round(pss(found_pids[name]), 1)
    found['seconds'] = round(elapsed, 3)
    return found


def need(response, what):
    """A request's result, or the run fails."""
    if 'result' not in response:
        raise RuntimeError(f'{what}: {response.get("error")}')
    return response['result']


def ended(connection, handle):
    """Wait for a turn, which must end completed, or the run fails."""
    found = need(connection.request('wait', handles=[handle], timeout_ms=60_000),
                 'wait')['results'][handle]
    if found.get('pending') or found.get('status') != 'completed':
        raise RuntimeError(f'turn: {found}')


def spread(values):
    if not values:
        return {'n': 0}
    values = sorted(values)
    return {'n': len(values), 'p50': round(statistics.median(values), 2),
            'p99': round(values[min(len(values) - 1, int(len(values) * .99))], 2),
            'max': round(values[-1], 2), 'over_100ms': sum(v > 100 for v in values)}


def connect(sock, process, timeout=60):
    deadline = time.monotonic() + timeout
    while True:
        try:
            return Connection(sock, retain_durable=False)
        except (FileNotFoundError, ConnectionRefusedError):
            if process.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError('daemon startup failed')
            time.sleep(.02)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--agent', required=True)
    parser.add_argument('--sonny', required=True)
    parser.add_argument('--end', choices=('low', 'high'), required=True)
    parser.add_argument('--out', required=True)
    parser.add_argument('--agents', type=int, default=10)
    parser.add_argument('--stored', type=int, default=10_000)
    parser.add_argument('--windows', type=int, default=32)
    parser.add_argument('--layout', choices=('one', 'spread'), default='one')
    parser.add_argument('--open', choices=('idle', 'busy'), default='idle',
                        help='open the windows before the fleet starts, or while it runs')
    parser.add_argument('--busy', type=int, default=30)
    parser.add_argument('--scripts', type=int, default=8)
    parser.add_argument('--seconds', type=float, default=20)
    parser.add_argument('--probes', type=int, default=300)
    parser.add_argument('--pace', type=float, default=0,
                        help='seconds each busy turn and each script take at least, so both sides do the same work')
    parser.add_argument('--turns', type=int, default=30)
    parser.add_argument('--seed', help='a store of the stored agents to copy, made when missing')
    parser.add_argument('--dotted', action='store_true',
                        help="name each agent as its folder's project's, PROJECT.NAME, as Sonny names them")
    args = parser.parse_args()
    agent, sonny = (str(Path(p).resolve()) for p in (args.agent, args.sonny))
    # One fixed place, so a seed's folders are where its agents say.
    work = Path('/tmp/ends-work')
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir()
    home, store, sock = work / 'home', work / 'state.sqlite', work / 'd.sock'
    (home / '.agent').mkdir(parents=True)
    server, url = start()
    env = {'PATH': os.environ['PATH'], 'HOME': str(home), 'TERM': 'xterm-256color',
           'LANG': 'en_US.UTF-8'}
    windows = Windows()
    stopping = threading.Event()
    threads = []
    daemon = None
    try:
        # Folders: the low end's one; the high end's window folders (one
        # for a busy project, one each spread), then the rest.
        count = args.windows if args.end == 'high' else 1
        folders = [work / f'p{n}' for n in range(count if args.layout == 'spread' else 1)]
        for folder in folders:
            folder.mkdir()
            subprocess.run(['git', 'init', '-q', '-b', 'main', str(folder)], check=True)
            (folder / 'README').write_text('ends\n')
        seeded = args.seed and Path(args.seed).exists()
        if seeded:
            for suffix in ('', '-wal', '-shm'):
                if Path(args.seed + suffix).exists():
                    shutil.copy(args.seed + suffix, str(store) + suffix)

        def serve():
            process = subprocess.Popen(
                [agent, 'serve', '--store', str(store), '--provider', f'openai=responses,{url}',
                 '--socket', str(sock)], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                stderr=open(work / 'daemon.err', 'a'), env=env, start_new_session=True)
            return process, connect(sock, process)

        daemon, control = serve()
        # Stored agents keep their folder: a seed is made for these folders.
        names = []
        # Each agent as its folder's project's, with --dotted.
        def of(folder, name):
            return f'{folder.name}.{name}' if args.dotted else name
        busy_bot = lambda n: of(folders[0], f'b{n}')
        script_bot = lambda n: of(folders[0], f's{n}')
        if args.end == 'low':
            names = [(of(folders[0], 'lead'), folders[0], None)] + [(of(folders[0], f't{n}'), folders[0], None)
                                                                    for n in range(1, args.agents)]
        else:
            for n in range(args.windows):
                folder = folders[n if args.layout == 'spread' else 0]
                names.append((of(folder, f'w{n}'), folder, None))
            names += [(busy_bot(n), folders[0], None) for n in range(args.busy)]
            names += [(script_bot(n), folders[0], f'h{n}') for n in range(args.scripts)]
            names += [(of(folders[0], f'x{n}'), folders[0], None) for n in range(max(0, args.stored - len(names)))]
        # The filler first, so a seed holds only it: every run makes its
        # windows', busy and script agents itself.
        filler = [n for n in names if n[0].rsplit('.', 1)[-1].startswith('x')]
        made = time.monotonic()
        for name, folder, tag in ([] if seeded else filler) + [n for n in names if n not in filler]:
            extra = {'declare': [dict(SCRIPT, answered_by=tag)]} if tag else {}
            created = control.request('create', bot=name, workspace=str(folder), **extra)
            if 'error' in created:
                raise RuntimeError(created)
            if args.seed and not seeded and name == filler[-1][0]:
                control.request('shutdown', grace_ms=0)
                daemon.wait(60)
                shutil.copy(store, args.seed)
                daemon, control = serve()
        made = time.monotonic() - made
        result = {'end': args.end, 'agent': agent, 'stored': len(names), 'created_s': round(made, 1)}

        def window(n, name, folder):
            return windows.open(n, [sonny, '--store', str(store), '--socket', str(sock), '--agent', name,
                                    '--no-sound', '--no-layout'], env, folder)

        if args.end == 'low':
            # One window, opened on its lead.
            window('lead', names[0][0], folders[0])
            started = windows.drawn('lead')
            time.sleep(8)  # its first reads done, the lookout started
            groups = {'daemon': lambda: [daemon.pid],
                      'window': lambda: tree(windows.pids['lead'][0])}
            result['window_start_s'] = round(started, 3) if started else None
            result['idle'] = measured(groups, args.seconds, daemon.pid, control)
            # A key with no echo in two seconds counts as two seconds, so a
            # freeze shows.
            result['key_ms'] = spread([t if t is not None else 2000.0
                                       for t in (windows.probe('lead') for _ in range(40))])
            turns = []
            for n in range(args.turns):
                began = time.monotonic()
                submitted = need(control.request('submit', bot=names[1][0], request_id=f'turn{n}',
                                                 prompt='hi'), 'submit')
                ended(control, f'{names[1][0]}/{submitted["message"]}')
                turns.append((time.monotonic() - began) * 1000)
            result['turn_ms'] = spread(turns)
        else:
            finished = [0, 0]
            failures = []

            def busy_still(submitted):
                # Only a bot still ending its last turn is tried again;
                # any other refusal fails the run.
                if 'result' in submitted:
                    return False
                error = submitted.get('error')
                if (error.get('code') if isinstance(error, dict) else error) == 'bot_busy':
                    return True
                raise RuntimeError(f'submit: {submitted.get("error")}')

            def worker(run, name):
                try:
                    run()
                except Exception as error:  # the measurement fails below
                    failures.append(f'{name}: {error}')
                    stopping.set()

            def busy(n):
                worker(lambda: run_busy(n), busy_bot(n))

            def run_busy(n):
                connection = Connection(sock, retain_durable=False)
                turn = 0
                while not stopping.is_set():
                    turn += 1
                    due = time.monotonic() + args.pace
                    submitted = connection.request('submit', bot=busy_bot(n), request_id=f'r{turn}', prompt='delay:50')
                    if busy_still(submitted):
                        time.sleep(.05)
                        continue
                    ended(connection, f'{busy_bot(n)}/{submitted["result"]["message"]}')
                    finished[0] += 1
                    stopping.wait(max(0, due - time.monotonic()))
                connection.close()

            def script(n):
                worker(lambda: run_script(n), script_bot(n))

            def run_script(n):
                connection = Connection(sock, retain_durable=False)
                bot, tag, turn = script_bot(n), f'h{n}', 0
                while not stopping.is_set():
                    turn += 1
                    due = time.monotonic() + args.pace
                    submitted = connection.request('submit', bot=bot, request_id=f'r{turn}', prompt='call:exec')
                    if busy_still(submitted):
                        time.sleep(.05)
                        continue
                    call = None
                    while call is None and not stopping.is_set():
                        listed = connection.request('tool_calls', tag=tag)['result']['calls']
                        call = next((c for c in listed if c['bot'] == bot), None)
                        if call is None:
                            time.sleep(.01)
                    if call is None:
                        break
                    at = dict(bot=bot, turn=call['turn'], call_id=call['call_id'])
                    need(connection.request('take_call', **at), 'take_call')
                    for step in ('1', '2', '3'):
                        decided = need(connection.request('approve_step', **at, step=step,
                                                          name='mcp__docs__read',
                                                          arguments={'path': step}), 'approve_step')
                        if decided.get('decision') != 'allow':
                            raise RuntimeError(f'approve_step: {decided}')
                        need(connection.request('step_result', **at, step=step), 'step_result')
                    need(connection.request('tool_result', **at, output='done'), 'tool_result')
                    ended(connection, f'{bot}/{submitted["result"]["message"]}')
                    finished[1] += 1
                    stopping.wait(max(0, due - time.monotonic()))
                connection.close()

            def run_fleet():
                threads.extend(threading.Thread(target=busy, args=(n,)) for n in range(args.busy))
                threads.extend(threading.Thread(target=script, args=(n,)) for n in range(args.scripts))
                for thread in threads:
                    thread.start()
                time.sleep(3)

            # The windows open on an idle fleet, or (`--open busy`) on one
            # already running its turns and scripts, so their start is
            # measured under load.
            if args.open == 'busy':
                run_fleet()
            for n in range(args.windows):
                name, folder, _ = names[n]
                window(name, name, folder)
                time.sleep(.05)
            starts = [windows.drawn(name) for name, _, _ in names[:args.windows]]
            result['windows_undrawn'] = sum(s is None for s in starts)
            starts = [s * 1000 for s in starts if s is not None]
            result['window_start_ms'] = spread(starts)
            time.sleep(10)
            if args.open == 'idle':
                run_fleet()
            names_open = [name for name, _, _ in names[:args.windows]]
            groups = {'daemon': lambda: [daemon.pid],
                      'windows': lambda: tree(*(windows.pids[name][0] for name in names_open))}
            keys, began = [], time.monotonic()
            usage_box = {}
            meter = threading.Thread(target=lambda: usage_box.update(measured(groups, args.seconds, daemon.pid, control)))
            meter.start()
            n = 0
            while len(keys) < args.probes and time.monotonic() - began < args.seconds * 4:
                took = windows.probe(names_open[n % len(names_open)])
                keys.append(took if took is not None else 2000.0)
                n += 1
                time.sleep(.02)
            meter.join()
            result['busy'] = usage_box
            result['key_ms'] = spread(keys)
            stopping.set()
            for thread in threads:
                thread.join(timeout=120)
            if failures:
                raise RuntimeError('workers failed: ' + '; '.join(failures))
            if (args.busy and not finished[0]) or (args.scripts and not finished[1]):
                raise RuntimeError(f'no work done: {finished}')
            result['turns_finished'] = finished[0]
            result['scripts_finished'] = finished[1]
            result.update(windows=args.windows, layout=args.layout, open=args.open, busy_agents=args.busy,
                          script_agents=args.scripts, pace_s=args.pace)
        Path(args.out).write_text(json.dumps(result, indent=1) + '\n')
        print(json.dumps(result))
    finally:
        stopping.set()
        windows.close()
        if daemon and daemon.poll() is None:
            daemon.terminate()
            daemon.wait(30)
        server.shutdown()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == '__main__':
    main()
