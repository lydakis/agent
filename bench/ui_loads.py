"""How fast Sonny's pages and rows load, at 50, 5,000 and 10,000 agents
(George, 11-25): from the key to what the person waits to see, in a real
pseudo-terminal, on a daemon of its own, a synthetic model and a scratch
HOME. Nothing of the person's is read or touched.

Each size is one store of P projects of A agents (a lead `pN.lead` in its
own folder and its tasks `pN.tK`), every agent with one finished turn
`hello from NAME`, every third task closed:

    50       5 projects of 10
    5,000    50 projects of 100
    10,000   100 projects of 100

A store is seeded once by the daemon and kept under --seeds; every run
works on a fresh copy of it. WORKING tasks (a few in the window's own
project, the rest spread over the others) hold a turn open throughout, so
the counts every page shows are known and stay still.

Components, each timed from the key (or the start) to the chunk of output
that made the screen show it; a moment is stamped as the bytes arrive, not
on a poll:

  start       the window on p0.lead: its composer, the lead's conversation,
              its status bar under the composer filled (working and idle
              counts)
  projects    ← on the empty composer: the first project row, the head's
              working count across every project; first time, again
  pick        ↓ in Projects: the next row picked
  lead        Enter on another lead in Projects, back and forth: its
              conversation, its status bar filled
  switch      /switch: the list, then each typed letter's narrowed list
  status      /status: its card with the project's working count
  usage, background, commons, sky: each page's first full draw
  bar         a task of the window's project starts a turn, then ends it:
              until the status bar's working count says so
  typing      key to character while BUSY tasks run turns back to back
  idle        the window at rest: CPU, memory
  window2     a second window (a new process, as after an upgrade or a
              terminal tab) on p1.lead: its start and its first Projects

With every timed step: the CPU seconds the window and the daemon spent
until both were quiet. One more pass through a counting socket
(bench.sonny_screen.Tap) gives the daemon requests by kind and bytes back;
it is a pass of its own since a Python proxy slows every read it counts.

Given several builds (--build NAME=DIR, DIR holding `agent` and `sonny`),
rounds alternate between them on the same seed, each build's binaries
copied fresh together so neither reads warmer from the page cache, and
the summary sets them side by side.

    python3 -m bench.ui_loads --build main=target/release --sizes 50,5000,10000 --out .local/ui
    python3 -m bench.ui_loads --build main=.local/main --build head=target/release --rounds 3 --out .local/ui
    python3 -m bench.ui_loads --report .local/ui
"""
import argparse
import json
import os
import shutil
import signal
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.ends import connect, need  # noqa: E402
from bench.lookout_scale import cpu_seconds, rss_mib  # noqa: E402
from bench.sonny_screen import Screen, Tap  # noqa: E402
from bench.synthetic_model import start  # noqa: E402

SIZES = {50: (5, 10), 5000: (50, 100), 10000: (100, 100)}
LEFT, RIGHT, UP, DOWN, ESC, ENTER = b'\x1b[D', b'\x1b[C', b'\x1b[A', b'\x1b[B', b'\x1b', b'\r'
WORK = Path('/tmp/ui-loads')
COLUMNS, ROWS = 150, 40


def title(n):
    return f'Proj {n:03}'


class Term(Screen):
    """A program in a pseudo-terminal that answers the device query a real
    terminal answers, and stamps the moment each awaited thing shows: every
    chunk read is fed to the screen and the awaited tests run on it, the
    moment being when the chunk arrived."""

    def __init__(self, argv, env, cwd):
        self.watches, self.stamp_lock = [], threading.Lock()
        super().__init__(argv, env, cwd, columns=COLUMNS, rows=ROWS)

    def drain(self):
        import pyte
        while True:
            try:
                chunk = os.read(self.fd, 1 << 16)
            except OSError:
                return
            if not chunk:
                return
            at = time.monotonic()
            if b'\x1b[c' in chunk:
                os.write(self.fd, b'\x1b[?62;22c')
            if b'\x1b[6n' in chunk:
                os.write(self.fd, b'\x1b[1;1R')
            self.drawn += len(chunk)
            self.last = (self.last + chunk)[-4096:]
            with self.lock:
                try:
                    self.stream.feed(chunk)
                except Exception:  # noqa: BLE001 - a sequence pyte cannot take
                    self.stream = pyte.ByteStream(self.screen)
                lines = list(self.screen.display)
            with self.stamp_lock:
                for watch in self.watches:
                    for name, test in watch.tests.items():
                        if name not in watch.seen and test(lines):
                            watch.seen[name] = at
                            watch.lines[name] = lines

    def expect(self, **tests):
        """Watch for each test (over the screen's lines) to hold, from now."""
        watch = Watch(self, tests)
        with self.stamp_lock:
            self.watches.append(watch)
        # What already shows counts as shown now.
        lines = self.shown()
        for name, test in tests.items():
            if test(lines):
                watch.seen[name] = time.monotonic()
        return watch

    def press(self, keys, **tests):
        """Press keys, timing each test from the moment they went."""
        watch = self.expect(**tests)
        watch.seen.clear()
        watch.began = time.monotonic()
        os.write(self.fd, keys)
        return watch


class Watch:
    def __init__(self, term, tests):
        self.term, self.tests, self.seen, self.lines = term, tests, {}, {}
        self.began = time.monotonic()

    def wait(self, limit=120, need_all=True):
        """Seconds from the start to each test holding; one that never held
        in `limit` is None, or the run fails when `need_all`."""
        deadline = time.monotonic() + limit
        while time.monotonic() < deadline and len(self.seen) < len(self.tests):
            self.term.check('waiting', drawn=False)
            time.sleep(.01)
        with self.term.stamp_lock:
            self.term.watches.remove(self)
        missing = [name for name in self.tests if name not in self.seen]
        if missing and need_all:
            raise RuntimeError(f'no {missing} in {limit} s; the screen:\n' + '\n'.join(self.term.shown()))
        return {name: (round((self.seen[name] - self.began) * 1000, 1) if name in self.seen else None)
                for name in self.tests}


def has(*words):
    return lambda lines: any(all(w in line for w in words) for line in lines)


def lacks(*words):
    return lambda lines: not any(all(w in line for w in words) for line in lines)


def both(*tests):
    return lambda lines: all(test(lines) for test in tests)


def bar(working, idle=None, exact=False):
    """The status bar under the composer shows these counts: the row
    after the composer's last line holds `● working` (and `○ idle`). One
    more working counts too: the lead's own turn as its window opens (the
    upgrade note a fresh HOME's first lead gets) is rightly counted until
    it ends."""
    def test(lines):
        return one(working, lines) or (not exact and one(working + 1, lines))

    def one(working, lines):
        for at, line in enumerate(lines):
            if '›' in line[:3] or 'Message' in line:
                for below in lines[at + 1:at + 4]:
                    shows = f'● {working}' in below if working else '○' in below and '●' not in below
                    if shows and (idle is None or f'○ {idle}' in below):
                        return True
        return False
    return test


def quiet(pids, limit=120, still=.03):
    """Seconds until half a second passes in which `pids` together spend
    under `still` CPU seconds."""
    began = time.monotonic()
    last = sum(cpu_seconds(p) for p in pids)
    while time.monotonic() - began < limit:
        time.sleep(.5)
        now = sum(cpu_seconds(p) for p in pids)
        if now - last < still:
            break
        last = now
    return round(time.monotonic() - began - .5, 2)


def evict(path):
    """Drop a file from the page cache, so each build starts as cold."""
    try:
        fd = os.open(path, os.O_RDONLY)
        os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        os.close(fd)
    except OSError:
        pass


class Fleet:
    """One daemon on a copy of a size's store, the projects' folders, a
    scratch HOME, and the synthetic model."""

    def __init__(self, agent, size, seeds, work):
        self.agent, self.size = agent, size
        self.projects, self.per = SIZES[size]
        self.work = work
        shutil.rmtree(work, ignore_errors=True)
        work.mkdir(parents=True)
        self.home, self.store, self.sock = work / 'home', work / 'state.sqlite', work / 'd.sock'
        (self.home / '.agent').mkdir(parents=True)
        (self.home / '.agent/config.toml').write_text('[sonny]\nchime = false\nsuggest = false\n')
        self.server, self.url = start()
        self.env = {'PATH': os.environ['PATH'], 'HOME': str(self.home), 'TERM': 'xterm-256color',
                    'LANG': 'en_US.UTF-8'}
        self.folders = [work / 'w' / f'p{n}' for n in range(self.projects)]
        for n, folder in enumerate(self.folders):
            (folder / '.agents').mkdir(parents=True)
            (folder / '.agents/project.toml').write_text(f'name = "p{n}"\ntitle = "{title(n)}"\n')
            subprocess.run(['git', 'init', '-q', '-b', 'main', str(folder)], check=True)
        seed = Path(seeds) / f'{size}.sqlite'
        if seed.exists():
            shutil.copy(seed, self.store)
            self.serve()
        else:
            self.serve()
            self.seed()
            self.control.request('shutdown', grace_ms=0)
            self.daemon.wait(120)
            seed.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(self.store, seed)
            self.serve()

    def serve(self):
        self.daemon = subprocess.Popen(
            [self.agent, 'serve', '--store', str(self.store), '--provider', f'openai=responses,{self.url}',
             '--socket', str(self.sock)], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=open(self.work / 'daemon.err', 'a'), env=self.env, start_new_session=True)
        self.control = connect(self.sock, self.daemon)

    def seed(self):
        made = time.monotonic()
        handles = []
        for n, folder in enumerate(self.folders):
            lead = f'p{n}.lead'
            got = need(self.control.request('create', bot=lead, workspace=str(folder)), 'create')
            lead_id = got.get('bot_id') or got.get('bot', {}).get('bot_id')
            names = [lead]
            for t in range(self.per - 1):
                need(self.control.request('create', bot=f'p{n}.t{t}', workspace=str(folder), created_by=lead,
                                          created_by_id=lead_id), 'create')
                names.append(f'p{n}.t{t}')
            for name in names:
                sent = need(self.control.request('submit', bot=name, request_id=f'{name}-1',
                                                 prompt=f'hello from {name}'), 'submit')
                handles.append(f'{name}/{sent["message"]}')
        waiting = list(handles)
        while waiting:
            batch, waiting = waiting[:64], waiting[64:]
            found = need(self.control.request('wait', handles=batch, timeout_ms=3_000), 'wait')
            for handle, result in found['results'].items():
                if result.get('pending'):
                    waiting.append(handle)
                elif result.get('status') != 'completed':
                    raise RuntimeError(f'turn {handle}: {result}')
        for n in range(self.projects):
            for t in range(0, self.per - 1, 3):
                need(self.control.request('close', bot=f'p{n}.t{t}'), 'close')
        print(f'seeded {len(handles)} agents in {time.monotonic() - made:.0f} s', file=sys.stderr)

    def hold(self, own, spread):
        """Tasks that work throughout: `own` in p0, `spread` over the
        other projects; open tasks only (closed ones are every third)."""
        open_tasks = [t for t in range(self.per - 1) if t % 3]
        held = [f'p0.t{t}' for t in open_tasks[:own]]
        others = list(range(1, self.projects)) or [0]
        for k in range(spread):
            n = others[k % len(others)]
            t = open_tasks[(k // len(others) + (own if n == 0 else 0)) % len(open_tasks)]
            held.append(f'p{n}.t{t}')
        held = list(dict.fromkeys(held))
        for bot in held:
            need(self.control.request('submit', bot=bot, request_id=f'{bot}-hold', prompt='hold: work'), 'submit')
        self.held = held
        return held

    def window(self, lead='p0.lead', project=0, sock=None):
        return Term([self.sonny, '--store', str(self.store), '--socket', str(sock or self.sock),
                     '--agent', lead, '--no-sound', '--no-layout'], self.env, str(self.folders[project]))

    def close(self):
        try:
            self.server.release.set()
        except AttributeError:
            pass
        try:
            self.control.request('shutdown', grace_ms=0)
        except Exception:  # noqa: BLE001
            pass
        try:
            self.daemon.wait(30)
        except subprocess.TimeoutExpired:
            self.daemon.kill()
        self.server.shutdown()
        subprocess.run(['pkill', '-f', str(self.work) + '/'], check=False)


def step(window, daemon, keys, limit=300, need_all=True, **tests):
    """Press keys; the time to each test, and the CPU both spent until quiet."""
    cpu = (cpu_seconds(window.pid), cpu_seconds(daemon.pid))
    watch = window.press(keys, **tests)
    times = watch.wait(limit, need_all)
    quiet([window.pid, daemon.pid])
    times['window_cpu_ms'] = round((cpu_seconds(window.pid) - cpu[0]) * 1000)
    times['daemon_cpu_ms'] = round((cpu_seconds(daemon.pid) - cpu[1]) * 1000)
    return times


def back(window):
    """Esc until the conversation shows with nothing over it."""
    os.write(window.fd, ESC)
    time.sleep(.3)
    window.expect(composer=has('Message')).wait(30)
    time.sleep(.5)


def run(build, size, args, tapped=False):
    """One round of every component on one build at one size."""
    work = WORK / f'{size}'
    fleet = Fleet(build['agent'], size, args.seeds, work)
    fleet.sonny = build['sonny']
    result = {'build': build['name'], 'size': size, 'tapped': tapped}
    tap = None
    window = None
    try:
        held = fleet.hold(args.own, args.spread)
        own = sum(1 for bot in held if bot.startswith('p0.'))
        working = len(held)
        # p0's open agents: the lead and two of every three tasks; resting
        # are those not held.
        sock = None
        if tapped:
            sock = work / 't.sock'
            tap = Tap(fleet.sock, sock)
        daemon = fleet.daemon
        began = time.monotonic()
        window = fleet.window(sock=sock)
        watch = window.expect(composer=has('Message'), conversation=has('hello from p0.lead'),
                              bar=bar(own))
        watch.began = began
        result['start'] = watch.wait(300)
        quiet([window.pid, daemon.pid])
        time.sleep(2)
        quiet([window.pid, daemon.pid])
        result['rss_start_mib'] = round(rss_mib(window.pid), 1)
        counts = (lambda: tap.counts()) if tap else (lambda: ({}, 0, 0))

        def timed(name, keys, **tests):
            ops, _, read = counts()
            got = step(window, daemon, keys, **tests)
            ops2, _, read2 = counts()
            if tap:
                got['requests'] = {op: ops2[op] - ops.get(op, 0) for op in ops2 if ops2[op] - ops.get(op, 0)}
                got['bytes_back_kib'] = round((read2 - read) / 1024, 1)
            result.setdefault(name, []).append(got)
            print(f'{build["name"]} {size} {name}: {got}', file=sys.stderr)
            return got

        # Projects: open, close, open again.
        head = has('every project', f'{working} working')
        last_row = has(title(min(fleet.projects - 1, 1)))
        for _ in range(args.repeat):
            timed('projects', LEFT, first_row=both(last_row, lacks('reading…')), head=head)
            # Moving the pick down and back up.
            timed('pick', DOWN, drawn=lambda lines: True)
            timed('pick', UP, drawn=lambda lines: True)
            back(window)
        # Switching leads from Projects: to a lead this window has not
        # shown yet (↓ k times from p0 to pk), then back to p0, which it has.
        for k in range(1, min(args.repeat, fleet.projects - 1) + 1):
            for lead, keys, name in ((f'p{k}.lead', DOWN * k, 'lead_new'), ('p0.lead', UP * k, 'lead_back')):
                window.press(LEFT, page=head).wait(120)
                time.sleep(.3)
                for key in range(len(keys) // len(DOWN)):
                    os.write(window.fd, keys[:len(DOWN)])
                    time.sleep(.1)
                time.sleep(.3)
                own_here = sum(1 for bot in held if bot.startswith(lead.split('.')[0] + '.'))
                timed(name, ENTER, conversation=has(f'hello from {lead}'), bar=bar(own_here))
        # /switch: open, then type to narrow.
        for _ in range(args.repeat):
            timed('switch', b'/switch\r', listed=has('p0.t1'))
            for letter in b'p1.t':
                timed('switch_type', bytes([letter]), drawn=lambda lines: True)
            timed('switch_type', b'2', drawn=lambda lines: True, narrowed=both(has('p1.t2'), lacks('p0.t1')))
            back(window)
            back(window)
        # The pages.
        for _ in range(args.repeat):
            timed('status', b'/status\r', card=has(f'{own} working') if own else has('idle'))
            back(window)
        for name, ready in (('usage', has('By model')), ('background', has('Follows')),
                            ('commons', has('threads')),
                            ('sky', both(has('p0 lead'), lacks('Catching up')))):
            for _ in range(args.repeat):
                timed(name, f'/{name}\r'.encode(), page=ready)
                back(window)
        # The status bar following a task: one of p0's resting open tasks
        # takes a turn of eight seconds.
        idle_task = next(f'p0.t{t}' for t in range(fleet.per - 1) if t % 3 and f'p0.t{t}' not in held)
        for k in range(args.repeat):
            watch = window.expect(up=bar(own + 1, exact=True))
            sent = need(fleet.control.request('submit', bot=idle_task, request_id=f'bar-{k}',
                                              prompt='delay:8000'), 'submit')
            up = watch.wait(10, need_all=False)['up']
            handle = f'{idle_task}/{sent["message"]}'
            watch = window.expect(down=bar(own, exact=True))
            while need(fleet.control.request('wait', handles=[handle], timeout_ms=2_000),
                       'wait')['results'][handle].get('pending'):
                pass
            watch.began = time.monotonic()
            down = watch.wait(30, need_all=False)['down']
            result.setdefault('bar', []).append({'started_ms': up, 'ended_ms': down})
            time.sleep(1)
        # At rest.
        quiet([window.pid, daemon.pid])
        cpu, ops, _, read = cpu_seconds(window.pid), *counts()
        time.sleep(args.idle)
        ops2, _, read2 = counts()
        result['idle'] = {'window_cpu_pct': round((cpu_seconds(window.pid) - cpu) / args.idle * 100, 2),
                          'rss_mib': round(rss_mib(window.pid), 1)}
        if tap:
            result['idle']['requests_per_min'] = round(
                (sum(ops2.values()) - sum(ops.values())) * 60 / args.idle, 1)
            result['idle']['bytes_back_kib_per_min'] = round((read2 - read) / 1024 * 60 / args.idle, 1)
        # Typing while BUSY tasks run turns back to back.
        if args.busy:
            result['typing'] = typing(fleet, window, held, args)
        window.close()
        window = None
        # A second window, a new process, on p1.lead.
        began = time.monotonic()
        window = fleet.window('p1.lead', 1, sock=sock)
        own1 = sum(1 for bot in held if bot.startswith('p1.'))
        watch = window.expect(composer=has('Message p1 lead'), bar=bar(own1))
        watch.began = began
        result['window2_start'] = watch.wait(300)
        quiet([window.pid, daemon.pid])
        timed('window2_projects', LEFT, first_row=both(has(title(0)), lacks('reading…')), head=head)
    finally:
        if window:
            window.close()
        fleet.close()
    return result


def typing(fleet, window, held, args):
    """Key to character, in ms, while BUSY open tasks no one holds run
    turns back to back."""
    stop, turns = threading.Event(), [0]
    free = [f'p{n}.t{t}' for n in range(fleet.projects - 1, 1, -1) for t in range(fleet.per - 1)
            if t % 3 and f'p{n}.t{t}' not in held] or [f'p0.t{t}' for t in range(fleet.per - 1) if t % 3]

    def work(bot):
        connection = connect(fleet.sock, fleet.daemon)
        k = 0
        while not stop.is_set():
            k += 1
            sent = connection.request('submit', bot=bot, request_id=f'{bot}-b{k}', prompt='delay:500')
            if 'result' not in sent:
                # Its last turn has not let go yet.
                time.sleep(.1)
                continue
            sent = sent['result']
            handle = f'{bot}/{sent["message"]}'
            while not stop.is_set():
                found = need(connection.request('wait', handles=[handle], timeout_ms=2_000), 'wait')
                if not found['results'][handle].get('pending'):
                    break
            turns[0] += 1
        connection.close()
    workers = [threading.Thread(target=work, args=(bot,), daemon=True) for bot in free[:args.busy]]
    for worker in workers:
        worker.start()
    time.sleep(3)
    took = []
    cpu = (cpu_seconds(window.pid), cpu_seconds(fleet.daemon.pid))
    began = time.monotonic()
    for k in range(args.keys):
        letter = 'abcdefghij'[k % 10]
        typed = 'x' + ''.join('abcdefghij'[j % 10] for j in range(k + 1))
        watch = window.press(letter.encode(), shown=has(typed[-12:] if len(typed) > 12 else typed[1:]))
        got = watch.wait(5, need_all=False)['shown']
        took.append(got if got is not None else 5000)
        time.sleep(.05)
    spent = time.monotonic() - began
    stop.set()
    for worker in workers:
        worker.join(10)
    os.write(window.fd, b'\x15')  # ctrl+u: the composer empty again
    took.sort()
    return {'busy': len(workers), 'keys': len(took), 'p50_ms': round(statistics.median(took), 1),
            'p99_ms': took[min(len(took) - 1, int(len(took) * .99))], 'max_ms': took[-1],
            'over_100ms': sum(t > 100 for t in took),
            'window_cpu_pct': round((cpu_seconds(window.pid) - cpu[0]) / spent * 100, 1),
            'daemon_cpu_pct': round((cpu_seconds(fleet.daemon.pid) - cpu[1]) / spent * 100, 1)}


def builds(specs, out):
    """Each NAME=DIR's binaries, copied together under OUT so neither
    build's files are newer or warmer than the other's."""
    found = []
    for spec in specs:
        name, folder = spec.split('=', 1)
        into = Path(out) / 'bin' / name
        into.mkdir(parents=True, exist_ok=True)
        for binary in ('agent', 'sonny'):
            shutil.copy(Path(folder) / binary, into / binary)
        found.append({'name': name, 'agent': str((into / 'agent').resolve()),
                      'sonny': str((into / 'sonny').resolve())})
    return found


PAGES = ('projects', 'switch', 'status', 'usage', 'background', 'commons', 'sky')


def report(out):
    """Every round under OUT, set side by side: each timed thing's median
    over rounds and repeats, per build and size."""
    rows = {}
    for file in sorted(Path(out).glob('round-*.json')):
        got = json.loads(file.read_text())
        key = (got['size'], got['build'])
        for part, value in got.items():
            if isinstance(value, dict) and part not in ('idle', 'typing'):
                value = [value]
            if isinstance(value, list):
                for at, entry in enumerate(value):
                    # A page's first open in the window, apart from its
                    # opens after.
                    name = part if part not in PAGES else f'{part} {"first" if at == 0 else "again"}'
                    for measure, ms in entry.items():
                        if isinstance(ms, (int, float)):
                            rows.setdefault((name, measure), {}).setdefault(key, []).append(ms)
            elif part in ('idle', 'typing'):
                pass
        for part in ('idle', 'typing'):
            for measure, value in (got.get(part) or {}).items():
                if isinstance(value, (int, float)):
                    rows.setdefault((part, measure), {}).setdefault(key, []).append(value)
    keys = sorted({k for found in rows.values() for k in found})
    lines = ['| what | ' + ' | '.join(f'{size:,} {build}' for size, build in keys) + ' |',
             '|---|' + '---|' * len(keys)]
    for (part, measure), found in rows.items():
        cells = []
        for key in keys:
            values = sorted(found.get(key, []))
            if not values:
                cells.append('')
            elif len(values) == 1:
                cells.append(f'{values[0]:g}')
            else:
                cells.append(f'{statistics.median(values):g} (p95 {values[min(len(values) - 1, int(len(values) * .95))]:g})')
        lines.append(f'| {part} {measure} | ' + ' | '.join(cells) + ' |')
    table = '\n'.join(lines)
    (Path(out) / 'summary.md').write_text(table + '\n')
    return table


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('--build', action='append', default=[], help='NAME=DIR holding agent and sonny')
    parser.add_argument('--sizes', default='50,5000,10000')
    parser.add_argument('--rounds', type=int, default=1)
    parser.add_argument('--repeat', type=int, default=3, help='times each component is timed in a round')
    parser.add_argument('--own', type=int, default=3, help="tasks of the window's project that work throughout")
    parser.add_argument('--spread', type=int, default=27, help='tasks of other projects that work throughout')
    parser.add_argument('--busy', type=int, default=30, help='tasks that run turns back to back while typing')
    parser.add_argument('--keys', type=int, default=100)
    parser.add_argument('--idle', type=float, default=20)
    parser.add_argument('--tap', action='store_true', help='also a pass counting daemon requests')
    parser.add_argument('--seeds', default='.local/ui-seeds')
    parser.add_argument('--report', help='only print the summary of the rounds under this folder')
    parser.add_argument('--out', default='.local/ui-loads')
    args = parser.parse_args()
    if args.report:
        print(report(args.report))
        return
    if not args.build:
        parser.error('--build NAME=DIR, at least one')
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    found = builds(args.build, out)
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))
    for size in (int(s) for s in args.sizes.split(',')):
        for round_ in range(args.rounds):
            order = found if round_ % 2 == 0 else found[::-1]
            for build in order:
                for binary in (build['agent'], build['sonny']):
                    evict(binary)
                got = run(build, size, args)
                (out / f'round-{size}-{build["name"]}-{round_}.json').write_text(json.dumps(got, indent=1))
        if args.tap:
            for build in found:
                got = run(build, size, args, tapped=True)
                (out / f'tap-{size}-{build["name"]}.json').write_text(json.dumps(got, indent=1))
    print(report(out))


if __name__ == '__main__':
    main()
